//! One image contract for native and exported IOSurface-backed LINEAR images.
//! This module is portable so query/create decisions can be tested on vmhost.
use ash::vk;
use sv_proto as p;

pub const MTLTEXTURE: u32 = 0x0002_0000;
pub fn format_ok(format: i32) -> bool {
    matches!(vk::Format::from_raw(format), vk::Format::B8G8R8A8_UNORM | vk::Format::B8G8R8A8_SRGB
        | vk::Format::R8G8B8A8_UNORM | vk::Format::R8G8B8A8_SRGB)
}
fn supported_flags(flags: u32) -> bool {
    flags & !(vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE).as_raw() == 0
}
fn supported_usage(usage: u32) -> bool {
    let allowed = vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST
        | vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::COLOR_ATTACHMENT;
    usage != 0 && usage & !allowed.as_raw() == 0
}

/// Driver success alone does not promise that the texture can be imported.
pub fn importable(features: u32, compatible: u32, samples: u32) -> bool {
    features & vk::ExternalMemoryFeatureFlags::IMPORTABLE.as_raw() != 0
        && compatible & MTLTEXTURE != 0 && samples & vk::SampleCountFlags::TYPE_1.as_raw() != 0
}

/// Rewrite a supported plain or host-allocation query to the same dedicated
/// texture storage used by create/allocate/bind. Other native queries stay native.
pub fn prepare_query(info: &mut p::VkPhysicalDeviceImageFormatInfo2) -> Result<bool, ()> {
    let external: Vec<u32> = info.p_next.iter().filter_map(|l| match l {
        p::PNextLink::VkPhysicalDeviceExternalImageFormatInfo(x) => Some(x.handle_type.0 as u32), _ => None,
    }).collect();
    if external.len() > 1 { return Err(()); }
    let imported = external.first().is_some_and(|t| *t == crate::shared::HOST_ALLOCATION || *t == MTLTEXTURE);
    if !external.is_empty() && !imported { return Ok(false); }
    let candidate = info.tiling.0 == vk::ImageTiling::LINEAR.as_raw()
        && info.type_.0 == vk::ImageType::TYPE_2D.as_raw() && format_ok(info.format.0);
    if !candidate { return if imported { Err(()) } else { Ok(false) }; }
    if !supported_flags(info.flags.0) || !supported_usage(info.usage.0) { return Err(()); }
    if external.is_empty() {
        let mut x = p::VkPhysicalDeviceExternalImageFormatInfo::default();
        x.handle_type = p::VkExternalMemoryHandleTypeFlagBits(MTLTEXTURE as i32);
        info.p_next.push(p::PNextLink::VkPhysicalDeviceExternalImageFormatInfo(Box::new(x)));
    } else {
        for l in &mut info.p_next {
            if let p::PNextLink::VkPhysicalDeviceExternalImageFormatInfo(x) = l {
                x.handle_type.0 = MTLTEXTURE as i32;
            }
        }
    }
    Ok(true)
}

/// The native texture wrapper currently represents one 2D level and layer.
pub fn constrain_properties(props: &mut p::VkImageFormatProperties) {
    props.max_extent.depth = props.max_extent.depth.min(1);
    props.max_mip_levels = props.max_mip_levels.min(1);
    props.max_array_layers = props.max_array_layers.min(1);
    props.sample_counts.0 &= vk::SampleCountFlags::TYPE_1.as_raw();
}

pub fn prepare_create(ci: &mut p::VkImageCreateInfo) -> Result<bool, ()> {
    let mut query = p::VkPhysicalDeviceImageFormatInfo2::default();
    query.format = ci.format; query.type_ = ci.image_type; query.tiling = ci.tiling;
    query.usage = ci.usage; query.flags = ci.flags;
    for l in &ci.p_next {
        if let p::PNextLink::VkExternalMemoryImageCreateInfo(x) = l {
            // An image imported here has exactly one supported handle type.
            if !matches!(x.handle_types.0, crate::shared::HOST_ALLOCATION | MTLTEXTURE) { return Ok(false); }
            let mut ext = p::VkPhysicalDeviceExternalImageFormatInfo::default();
            ext.handle_type.0 = x.handle_types.0 as i32;
            query.p_next.push(p::PNextLink::VkPhysicalDeviceExternalImageFormatInfo(Box::new(ext)));
        }
    }
    if !prepare_query(&mut query)? { return Ok(false); }
    if ci.extent.width == 0 || ci.extent.height == 0 || ci.extent.depth != 1
        || ci.mip_levels != 1 || ci.array_layers != 1 || ci.samples.0 != vk::SampleCountFlags::TYPE_1.as_raw() as i32 {
        return Err(());
    }
    if let Some(x) = ci.p_next.iter_mut().find_map(|l| match l {
        p::PNextLink::VkExternalMemoryImageCreateInfo(x) => Some(x), _ => None,
    }) { x.handle_types.0 = MTLTEXTURE; }
    else {
        let mut x = p::VkExternalMemoryImageCreateInfo::default(); x.handle_types.0 = MTLTEXTURE;
        ci.p_next.push(p::PNextLink::VkExternalMemoryImageCreateInfo(Box::new(x)));
    }
    Ok(true)
}

/// Metadata required to bind an existing surface to one imported 2D image.
#[derive(Clone, Copy, Debug)]
pub struct SurfaceLayout {
    pub width: usize, pub height: usize, pub pitch: usize,
    pub bpp: usize, pub fourcc: u32, pub allocation: usize,
}
pub fn compatible_surface(surface: SurfaceLayout, width: u32, height: u32, fourcc: u32,
    offset: u64, pitch: u64, span: u64) -> bool {
    let Some(row) = surface.width.checked_mul(surface.bpp) else { return false; };
    let Some(end) = surface.pitch.checked_mul(surface.height.saturating_sub(1)).and_then(|n| n.checked_add(row)) else { return false; };
    surface.width == width as usize && surface.height == height as usize
        && width != 0 && height != 0 && surface.bpp == 4 && surface.fourcc == fourcc
        && offset == 0 && u64::try_from(surface.pitch).ok() == Some(pitch)
        && surface.pitch >= row && end <= surface.allocation
        && span <= surface.allocation as u64 && surface.allocation <= isize::MAX as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    fn query(usage: vk::ImageUsageFlags) -> p::VkPhysicalDeviceImageFormatInfo2 {
        let mut q = p::VkPhysicalDeviceImageFormatInfo2::default();
        q.format.0 = vk::Format::B8G8R8A8_UNORM.as_raw(); q.type_.0 = vk::ImageType::TYPE_2D.as_raw();
        q.tiling.0 = vk::ImageTiling::LINEAR.as_raw(); q.usage.0 = usage.as_raw(); q
    }
    fn create(q: &p::VkPhysicalDeviceImageFormatInfo2) -> p::VkImageCreateInfo {
        let mut ci = p::VkImageCreateInfo::default(); ci.format=q.format;ci.image_type=q.type_;ci.tiling=q.tiling;
        ci.usage=q.usage;ci.flags=q.flags;ci.extent.width=17;ci.extent.height=9;ci.extent.depth=1;
        ci.mip_levels=1;ci.array_layers=1;ci.samples.0=1;ci
    }
    #[test] fn import_capability_requires_real_driver_importability_and_sample_support() {
        assert!(importable(vk::ExternalMemoryFeatureFlags::IMPORTABLE.as_raw(),MTLTEXTURE,1));
        assert!(!importable(vk::ExternalMemoryFeatureFlags::EXPORTABLE.as_raw(),MTLTEXTURE,1));
        assert!(!importable(vk::ExternalMemoryFeatureFlags::IMPORTABLE.as_raw(),crate::shared::HOST_ALLOCATION,1));
        assert!(!importable(vk::ExternalMemoryFeatureFlags::IMPORTABLE.as_raw(),MTLTEXTURE,4));
    }
    #[test] fn plain_query_and_create_choose_identical_dedicated_texture_storage() {
        let original=query(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC);
        let mut q=original.clone(); assert_eq!(prepare_query(&mut q),Ok(true));
        assert!(matches!(&q.p_next[0],p::PNextLink::VkPhysicalDeviceExternalImageFormatInfo(x) if x.handle_type.0 as u32==MTLTEXTURE));
        let mut ci=create(&original); assert_eq!(prepare_create(&mut ci),Ok(true));
        assert!(matches!(&ci.p_next[0],p::PNextLink::VkExternalMemoryImageCreateInfo(x) if x.handle_types.0==MTLTEXTURE));
        assert_eq!(q.usage,ci.usage);
        let mut props=p::VkImageFormatProperties::default();props.max_mip_levels=16;props.max_array_layers=64;
        props.max_extent.depth=128;props.sample_counts.0=15;constrain_properties(&mut props);
        assert_eq!((props.max_mip_levels,props.max_array_layers,props.max_extent.depth,props.sample_counts.0),(1,1,1,1));
    }
    #[test] fn input_depth_and_multilayer_are_refused_before_driver_calls() {
        for usage in [vk::ImageUsageFlags::INPUT_ATTACHMENT,vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::INPUT_ATTACHMENT] {
            let mut q=query(usage);let mut ci=create(&q);assert_eq!(prepare_query(&mut q),Err(()));assert_eq!(prepare_create(&mut ci),Err(()));
        }
        let mut ci=create(&query(vk::ImageUsageFlags::COLOR_ATTACHMENT));ci.array_layers=2;
        assert_eq!(prepare_create(&mut ci),Err(()));
    }
    #[test] fn existing_surface_import_requires_exact_shape_format_pitch_and_zero_offset() {
        let bgra = u32::from_be_bytes(*b"BGRA");
        let surface=SurfaceLayout{width:17,height:9,pitch:128,bpp:4,fourcc:bgra,allocation:1152};
        assert!(compatible_surface(surface,17,9,bgra,0,128,1152));
        for (width,height,format,offset,pitch,span) in [
            (18,9,bgra,0,128,1152),(17,8,bgra,0,128,1152),
            (17,9,u32::from_be_bytes(*b"RGBA"),0,128,1152),
            (17,9,bgra,64,128,1152),(17,9,bgra,0,256,1152),(17,9,bgra,0,128,1153),
        ] { assert!(!compatible_surface(surface,width,height,format,offset,pitch,span)); }
        assert!(!compatible_surface(SurfaceLayout{pitch:64,..surface},17,9,bgra,0,64,576));
        assert!(!compatible_surface(SurfaceLayout{bpp:1,..surface},17,9,bgra,0,128,1152));
    }
    #[test] fn other_native_formats_and_optimal_storage_are_not_rewritten() {
        let mut q=query(vk::ImageUsageFlags::COLOR_ATTACHMENT);q.tiling.0=vk::ImageTiling::OPTIMAL.as_raw();
        let original=q.clone();assert_eq!(prepare_query(&mut q),Ok(false));assert_eq!(q,original);
        q.tiling.0=vk::ImageTiling::LINEAR.as_raw();q.format.0=vk::Format::R16_UNORM.as_raw();
        assert_eq!(prepare_query(&mut q),Ok(false));assert!(q.p_next.is_empty());
    }
}
