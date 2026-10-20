//! Isolated native mutable-plane creation proof. No native work is submitted.
//! Views and conversion are destroyed before the exact joined image owner.
use super::*;
struct Views {
    driver: Arc<Driver>,
    // Retain the exact VkImage, allocation and IOSurface/Metal owners until all
    // associated views and conversion have actually been destroyed.
    _image: Arc<Image>,
    views: Vec<vk::ImageView>,
    conversion: Option<vk::SamplerYcbcrConversion>,
}
impl Drop for Views {
    fn drop(&mut self) {
        // This proof never submits native commands and never starts a producer.
        // Every owner can be released even after a partial view-create error.
        unsafe {
            for view in self.views.drain(..).rev() {
                self.driver.device.destroy_image_view(view, None);
            }
            if let Some(conversion) = self.conversion.take() {
                self.driver
                    .device
                    .destroy_sampler_ycbcr_conversion(conversion, None);
            }
        }
    }
}
fn stage<T>(name: &str, result: Result<T, vk::Result>) -> Result<T, String> {
    result.map_err(|error| format!("mutable NV12 {name}: {error:?}"))
}
pub(super) fn run() -> Result<(), String> {
    let driver = Arc::new(Driver::open(0).map_err(|error| error.to_string())?);
    let flags = vk::ImageCreateFlags::MUTABLE_FORMAT;
    let usage = vk::ImageUsageFlags::SAMPLED
        | vk::ImageUsageFlags::TRANSFER_SRC
        | vk::ImageUsageFlags::TRANSFER_DST;
    let formats = [
        vk::Format::G8_B8R8_2PLANE_420_UNORM,
        vk::Format::R8_UNORM,
        vk::Format::R8G8_UNORM,
    ];
    let image = stage(
        "query/create exact contract",
        Image::create_config(Arc::clone(&driver), 64, 64, flags, usage, &formats),
    )?;
    let contract = image.contract();
    if contract.flags() != flags
        || contract.usage() != usage
        || contract.view_formats().map(|value| value.as_slice()) != Some(formats.as_slice())
    {
        return Err("mutable NV12 retained contract differs from requested native contract".into());
    }
    let requirements = image.requirements();
    // One fixed 64x64 diagnostic owner. Refuse unexpectedly large native memory
    // requirements before allocating its joined target; no allocation retry.
    if requirements.size == 0 || requirements.size > 16 * 1024 * 1024 {
        return Err(format!(
            "mutable NV12 allocation requirement {} exceeds fixed 16MiB bound",
            requirements.size
        ));
    }
    let memory = stage("allocate exact joined target", image.allocate())?;
    stage("bind exact joined target", image.bind(memory, 0))?;
    stage("validate exact plane layouts", image.validate_bound())?;
    let picture = stage("retain original joined picture", image.picture())?;
    let layout = picture.layout();
    let mut owners = Views {
        driver: Arc::clone(&driver),
        _image: Arc::clone(&image),
        views: Vec::with_capacity(3),
        conversion: None,
    };
    for (plane, format, aspect) in [
        (0, vk::Format::R8_UNORM, vk::ImageAspectFlags::PLANE_0),
        (1, vk::Format::R8G8_UNORM, vk::ImageAspectFlags::PLANE_1),
    ] {
        if !contract.allows_view(format, aspect) {
            return Err(format!(
                "mutable NV12 plane{plane} not permitted by retained contract"
            ));
        }
        let view = stage(&format!("create plane{plane} view"), unsafe {
            driver.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image.image())
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: aspect,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
        })?;
        owners.views.push(view);
        println!(
            "mutable_nv12_plane plane={plane} native_image={:?} native_view={view:?} format={format:?} aspect={aspect:?} joined_offset={} joined_pitch={} same_exact_image=true",
            image.image(),
            layout.planes[plane].offset,
            layout.planes[plane].stride
        );
    }
    let ycbcr_enabled = driver.features.p_next.iter().any(|link| match link {
        p::PNextLink::VkPhysicalDeviceVulkan11Features(features) => {
            features.sampler_ycbcr_conversion
        }
        p::PNextLink::VkPhysicalDeviceSamplerYcbcrConversionFeatures(features) => {
            features.sampler_ycbcr_conversion
        }
        _ => false,
    });
    let format_properties = unsafe {
        driver
            .instance
            .get_physical_device_format_properties(driver.pd, formats[0])
    };
    let features = format_properties.linear_tiling_features;
    let chroma = if features.contains(vk::FormatFeatureFlags::MIDPOINT_CHROMA_SAMPLES) {
        Some(vk::ChromaLocation::MIDPOINT)
    } else if features.contains(vk::FormatFeatureFlags::COSITED_CHROMA_SAMPLES) {
        Some(vk::ChromaLocation::COSITED_EVEN)
    } else {
        None
    };
    let full_view = if ycbcr_enabled && chroma.is_some() {
        let chroma = chroma.unwrap();
        let conversion = stage("create full NV12 YCbCr conversion", unsafe {
            driver.device.create_sampler_ycbcr_conversion(
                &vk::SamplerYcbcrConversionCreateInfo::default()
                    .format(formats[0])
                    .ycbcr_model(vk::SamplerYcbcrModelConversion::YCBCR_601)
                    .ycbcr_range(vk::SamplerYcbcrRange::ITU_NARROW)
                    .components(vk::ComponentMapping::default())
                    .x_chroma_offset(chroma)
                    .y_chroma_offset(chroma)
                    .chroma_filter(vk::Filter::NEAREST)
                    .force_explicit_reconstruction(false),
                None,
            )
        })?;
        owners.conversion = Some(conversion);
        let mut conversion_info = vk::SamplerYcbcrConversionInfo::default().conversion(conversion);
        let view = stage("create full NV12 color view with conversion", unsafe {
            driver.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .push_next(&mut conversion_info)
                    .image(image.image())
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(formats[0])
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
        })?;
        owners.views.push(view);
        println!(
            "mutable_nv12_full native_image={:?} native_view={view:?} native_conversion={conversion:?} model=601 range=narrow chroma={chroma:?} filter=nearest",
            image.image()
        );
        true
    } else {
        return Err(format!(
            "mutable NV12 full conversion view unsupported: enabled YCbCr feature={ycbcr_enabled}, linear chroma location={chroma:?}"
        ));
    };
    let view_count = owners.views.len();
    // Drop all views/conversion before dropping the joined picture/image. No
    // command, queue submit, external decode or GPU sampling occurred here.
    drop(owners);
    drop(picture);
    drop(image);
    println!(
        "PASS mutable_nv12_views extent=64x64 flags=MUTABLE_FORMAT native_usage=SAMPLED|TRANSFER_SRC|TRANSFER_DST exact_view_formats=NV12,R8_UNORM,R8G8_UNORM linear_mtltexture_query_and_create=true joined_allocation_bytes={} allocation_bound=16777216 plane_views=2 full_conversion_view={full_view} native_views_destroyed={view_count} submitted_work=false pixel_validation=false readiness_changed=false",
        requirements.size
    );
    Ok(())
}
