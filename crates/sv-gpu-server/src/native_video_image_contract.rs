//! Portable validation of the real NV12 backing contract. Logical VIDEO usage
//! is validated/emulated by the caller and is never silently removed here.
use ash::vk;
pub(crate) const MUTABLE_VIEW_FORMATS: [vk::Format; 3] = [
    vk::Format::G8_B8R8_2PLANE_420_UNORM,
    vk::Format::R8_UNORM,
    vk::Format::R8G8_UNORM,
];
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Contract {
    flags: vk::ImageCreateFlags,
    usage: vk::ImageUsageFlags,
    formats: Option<[vk::Format; 3]>,
}
impl Contract {
    /// Existing native diagnostic path. No mutable flag or format-list chain.
    pub(crate) fn diagnostic() -> Self {
        Self {
            flags: vk::ImageCreateFlags::empty(),
            usage: vk::ImageUsageFlags::SAMPLED,
            formats: None,
        }
    }
    pub(crate) fn mutable(
        flags: vk::ImageCreateFlags,
        usage: vk::ImageUsageFlags,
        formats: &[vk::Format],
    ) -> Result<Self, vk::Result> {
        let allowed_usage = vk::ImageUsageFlags::SAMPLED
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST;
        if flags != vk::ImageCreateFlags::MUTABLE_FORMAT
            || !usage.contains(vk::ImageUsageFlags::SAMPLED)
            || !(usage & !allowed_usage).is_empty()
            || formats != MUTABLE_VIEW_FORMATS.as_slice()
        {
            return Err(vk::Result::ERROR_FORMAT_NOT_SUPPORTED);
        }
        Ok(Self {
            flags,
            usage,
            formats: Some(MUTABLE_VIEW_FORMATS),
        })
    }
    pub(crate) fn flags(self) -> vk::ImageCreateFlags {
        self.flags
    }
    pub(crate) fn usage(self) -> vk::ImageUsageFlags {
        self.usage
    }
    pub(crate) fn view_formats(&self) -> Option<&[vk::Format; 3]> {
        self.formats.as_ref()
    }
    /// Only a full NV12 color view is valid without MUTABLE_FORMAT. Plane view
    /// format and plane aspect must agree; no combined/other aspects are valid.
    pub(crate) fn allows_view(self, format: vk::Format, aspect: vk::ImageAspectFlags) -> bool {
        if format == vk::Format::G8_B8R8_2PLANE_420_UNORM && aspect == vk::ImageAspectFlags::COLOR {
            return true;
        }
        self.formats.is_some()
            && matches!(
                (format, aspect),
                (vk::Format::R8_UNORM, vk::ImageAspectFlags::PLANE_0)
                    | (vk::Format::R8G8_UNORM, vk::ImageAspectFlags::PLANE_1)
            )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_retains_original_native_shape() {
        let contract = Contract::diagnostic();
        assert!(contract.flags().is_empty());
        assert_eq!(contract.usage(), vk::ImageUsageFlags::SAMPLED);
        assert_eq!(contract.view_formats(), None);
        assert!(contract.allows_view(MUTABLE_VIEW_FORMATS[0], vk::ImageAspectFlags::COLOR));
        assert!(!contract.allows_view(vk::Format::R8_UNORM, vk::ImageAspectFlags::PLANE_0));
    }
    #[test]
    fn mutable_native_query_and_creation_keep_exact_requested_contract() {
        for transfer in [
            vk::ImageUsageFlags::empty(),
            vk::ImageUsageFlags::TRANSFER_SRC,
            vk::ImageUsageFlags::TRANSFER_DST,
            vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST,
        ] {
            let usage = vk::ImageUsageFlags::SAMPLED | transfer;
            let contract = Contract::mutable(
                vk::ImageCreateFlags::MUTABLE_FORMAT,
                usage,
                &MUTABLE_VIEW_FORMATS,
            )
            .unwrap();
            assert_eq!(contract.flags(), vk::ImageCreateFlags::MUTABLE_FORMAT);
            assert_eq!(contract.usage(), usage);
            assert_eq!(contract.view_formats(), Some(&MUTABLE_VIEW_FORMATS));
        }
    }
    #[test]
    fn disjoint_unknown_flags_and_logical_video_usage_are_not_stripped() {
        for flags in [
            vk::ImageCreateFlags::empty(),
            vk::ImageCreateFlags::DISJOINT,
            vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::DISJOINT,
            vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE,
            vk::ImageCreateFlags::from_raw(0x80000000),
        ] {
            assert!(
                Contract::mutable(flags, vk::ImageUsageFlags::SAMPLED, &MUTABLE_VIEW_FORMATS)
                    .is_err()
            );
        }
        for usage in [
            vk::ImageUsageFlags::empty(),
            vk::ImageUsageFlags::TRANSFER_SRC,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::STORAGE,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::from_raw(0x80000000),
        ] {
            assert!(
                Contract::mutable(
                    vk::ImageCreateFlags::MUTABLE_FORMAT,
                    usage,
                    &MUTABLE_VIEW_FORMATS
                )
                .is_err()
            );
        }
    }
    #[test]
    fn lists_and_plane_views_must_match_exact_native_formats() {
        for formats in [
            vec![],
            vec![MUTABLE_VIEW_FORMATS[0]],
            vec![
                MUTABLE_VIEW_FORMATS[0],
                vk::Format::R8_UNORM,
                vk::Format::R8_UNORM,
            ],
            vec![
                MUTABLE_VIEW_FORMATS[0],
                vk::Format::R8G8_UNORM,
                vk::Format::R8_UNORM,
            ],
            vec![
                MUTABLE_VIEW_FORMATS[0],
                vk::Format::R8_UNORM,
                vk::Format::R8G8_UNORM,
                vk::Format::R8G8B8A8_UNORM,
            ],
        ] {
            assert!(
                Contract::mutable(
                    vk::ImageCreateFlags::MUTABLE_FORMAT,
                    vk::ImageUsageFlags::SAMPLED,
                    &formats
                )
                .is_err()
            );
        }
        let contract = Contract::mutable(
            vk::ImageCreateFlags::MUTABLE_FORMAT,
            vk::ImageUsageFlags::SAMPLED,
            &MUTABLE_VIEW_FORMATS,
        )
        .unwrap();
        assert!(contract.allows_view(MUTABLE_VIEW_FORMATS[0], vk::ImageAspectFlags::COLOR));
        assert!(contract.allows_view(vk::Format::R8_UNORM, vk::ImageAspectFlags::PLANE_0));
        assert!(contract.allows_view(vk::Format::R8G8_UNORM, vk::ImageAspectFlags::PLANE_1));
        for (format, aspect) in [
            (vk::Format::R8_UNORM, vk::ImageAspectFlags::COLOR),
            (vk::Format::R8G8_UNORM, vk::ImageAspectFlags::PLANE_0),
            (vk::Format::R8_UNORM, vk::ImageAspectFlags::PLANE_1),
            (MUTABLE_VIEW_FORMATS[0], vk::ImageAspectFlags::PLANE_0),
            (
                vk::Format::R8_UNORM,
                vk::ImageAspectFlags::PLANE_0 | vk::ImageAspectFlags::PLANE_1,
            ),
        ] {
            assert!(!contract.allows_view(format, aspect));
        }
    }
}
