//! Headless query -> create -> requirements -> allocate -> bind hardware gate.
//! Cross-build on vmhost. Target execution requires separate authorization.
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("this probe requires macOS");
}

#[cfg(target_os = "macos")]
fn main() {
    use ash::vk;
    use sv_gpu_server::{driver::Driver, iosurface, metal_image};
    use sv_proto as p;
    let driver = Driver::open(0).expect("open native Vulkan driver");
    for format in [
        vk::Format::B8G8R8A8_UNORM,
        vk::Format::B8G8R8A8_SRGB,
        vk::Format::R8G8B8A8_UNORM,
        vk::Format::R8G8B8A8_SRGB,
    ] {
        for usage in [
            vk::ImageUsageFlags::COLOR_ATTACHMENT,
            vk::ImageUsageFlags::STORAGE,
            vk::ImageUsageFlags::COLOR_ATTACHMENT
                | vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::TRANSFER_DST,
        ] {
            let mut query = p::VkPhysicalDeviceImageFormatInfo2::default();
            query.format.0 = format.as_raw();
            query.type_.0 = vk::ImageType::TYPE_2D.as_raw();
            query.tiling.0 = vk::ImageTiling::LINEAR.as_raw();
            query.usage.0 = usage.as_raw();
            assert!(
                query.p_next.is_empty(),
                "ordinary application query has no external chain"
            );
            if metal_image::prepare_query(&mut query).is_err() {
                assert!(matches!(
                    format,
                    vk::Format::B8G8R8A8_SRGB | vk::Format::R8G8B8A8_SRGB
                ));
                println!("{format:?} {usage:?}: unsupported SRGB shader-write route refused");
                continue;
            }
            let handle = query
                .p_next
                .iter()
                .find_map(|l| match l {
                    p::PNextLink::VkPhysicalDeviceExternalImageFormatInfo(x) => {
                        Some(x.handle_type.0 as u32)
                    }
                    _ => None,
                })
                .unwrap();
            let mut ext = vk::PhysicalDeviceExternalImageFormatInfo::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::from_raw(handle));
            let qi = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(format)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::LINEAR)
                .usage(usage)
                .push_next(&mut ext);
            let mut external = vk::ExternalImageFormatProperties::default();
            let mut props = vk::ImageFormatProperties2::default().push_next(&mut external);
            let queried = unsafe {
                driver
                    .instance
                    .get_physical_device_image_format_properties2(driver.pd, &qi, &mut props)
            };
            if let Err(error) = queried {
                assert_eq!(
                    usage,
                    vk::ImageUsageFlags::STORAGE,
                    "color usage must succeed for this gate"
                );
                println!("{format:?} {usage:?}: native storage query refused {error:?}");
                continue;
            }
            assert!(
                external
                    .external_memory_properties
                    .external_memory_features
                    .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
            );
            assert!(
                iosurface::linear_extra(&driver, format)
                    & u64::from(vk::FormatFeatureFlags::COLOR_ATTACHMENT.as_raw())
                    != 0
            );
            let mut ci = p::VkImageCreateInfo::default();
            ci.format.0 = format.as_raw();
            ci.image_type.0 = vk::ImageType::TYPE_2D.as_raw();
            ci.tiling.0 = vk::ImageTiling::LINEAR.as_raw();
            ci.usage.0 = usage.as_raw();
            ci.extent.width = 17;
            ci.extent.height = 9;
            ci.extent.depth = 1;
            ci.mip_levels = 1;
            ci.array_layers = 1;
            ci.samples.0 = 1;
            assert_eq!(metal_image::prepare_create(&mut ci), Ok(true));
            let handles = ci
                .p_next
                .iter()
                .find_map(|l| match l {
                    p::PNextLink::VkExternalMemoryImageCreateInfo(x) => Some(x.handle_types.0),
                    _ => None,
                })
                .unwrap();
            let mut external_ci = vk::ExternalMemoryImageCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::from_raw(handles));
            let create = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D {
                    width: 17,
                    height: 9,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::LINEAR)
                .usage(usage)
                .push_next(&mut external_ci);
            let image =
                unsafe { driver.device.create_image(&create, None) }.expect("create queried image");
            iosurface::remember(image, format, 17, 9, usage, vk::ImageCreateFlags::empty());
            let mut dedicated = vk::MemoryDedicatedRequirements::default();
            let mut req = vk::MemoryRequirements2::default().push_next(&mut dedicated);
            unsafe {
                driver.device.get_image_memory_requirements2(
                    &vk::ImageMemoryRequirementsInfo2::default().image(image),
                    &mut req,
                )
            };
            let memory_types = req.memory_requirements.memory_type_bits;
            assert_eq!(
                dedicated.requires_dedicated_allocation,
                vk::TRUE,
                "allowed KK import patch requires dedicated allocation"
            );
            let allocation = iosurface::make(&driver, image, memory_types);
            let result = match allocation {
                Ok((section, memory, _)) => {
                    let result = unsafe { driver.device.bind_image_memory(image, memory, 0) };
                    // No submission occurred. Destroy the image, then memory, then texture/surface.
                    unsafe {
                        driver.device.destroy_image(image, None);
                        driver.device.free_memory(memory, None)
                    };
                    drop(section);
                    result.map_err(|e| format!("bind: {e:?}"))
                }
                Err(e) => {
                    unsafe { driver.device.destroy_image(image, None) };
                    Err(e)
                }
            };
            iosurface::forget(image);
            result.expect("allocate and bind native IOSurface storage");
            println!(
                "{format:?} {usage:?}: ordinary LINEAR query/create/dedicated allocation/bind passed"
            );
        }
    }
    for usage in [
        vk::ImageUsageFlags::INPUT_ATTACHMENT,
        vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::INPUT_ATTACHMENT,
    ] {
        let mut q = p::VkPhysicalDeviceImageFormatInfo2::default();
        q.format.0 = vk::Format::B8G8R8A8_UNORM.as_raw();
        q.type_.0 = vk::ImageType::TYPE_2D.as_raw();
        q.tiling.0 = vk::ImageTiling::LINEAR.as_raw();
        q.usage.0 = usage.as_raw();
        assert_eq!(metal_image::prepare_query(&mut q), Err(()));
    }
    println!("INPUT_ATTACHMENT refused; no render or sampling completion claimed");
}
