//! One driver view per video image and view description, kept until the image
//! is destroyed: a workaround for Intel's Windows driver (docs/video-decode.md
//! §7.11).
//!
//! # What it works around
//!
//! ffmpeg's Vulkan decoder creates an image view for each picture it decodes
//! into and destroys it once the picture is released (`ff_vk_decode_prepare_frame`,
//! `ff_vk_decode_free_frame`), while the DPB slot that picture set up stays
//! active for later decodes. The specification binds a slot's picture through
//! the view named in *each* call, so that is legal. On the guest, Intel's
//! `igvk64.dll` 101.7076 faulted in `vkCmdDecodeVideoKHR` twice, at
//! `igvk64.dll+0x2f185b`. Both times it read an object pointer that was no
//! longer one: `[rcx+0x78]` with `rcx` = 4, and with a non-canonical
//! `0x800006a722ca67ac`. Both times it was an IDR in a later loop of the clip,
//! after hundreds of good frames. Over the whole run the decoder's images,
//! bitstream buffers, session and parameters all stayed alive. The only
//! objects freed were these views, two per frame, and each new one landed at a
//! freed one's address (`SV_VIDEO_LOG`'s "the address of a destroyed view,
//! reused", 539 times before the fault). That is a use-after-free of a view
//! inside the driver, and what the freed memory happens to hold decides
//! whether, and how, it faults.
//!
//! # What it does
//!
//! For an image created with a video decode usage, the first
//! `vkCreateImageView` of a given description creates a driver view and keeps
//! it. Later identical requests are given the same driver view, and
//! `vkDestroyImageView` of it does not reach the driver. The views are destroyed
//! when their image is, before it (`server::destroy_image`, and
//! `Objects::destroy_leftovers` for a session that leaves them). So no driver
//! view of a video image is ever freed while its image lives, and Intel has
//! nothing stale to read.
//!
//! The application sees nothing: one view handle per call, as before. Two of
//! its handles may now name one driver view, which Vulkan does not forbid, and
//! which nothing on this side keys on. ffmpeg asks for a handful of
//! descriptions per image (the output and DPB views), so the cache is bounded
//! by the images, not by the frames. `SV_VIDEO_VIEW_CACHE=0` turns it off.

use std::collections::{HashMap, HashSet};

/// `VK_IMAGE_USAGE_VIDEO_DECODE_{DST,SRC,DPB}_BIT_KHR`.
pub const VIDEO_DECODE_USAGE: u32 = 0x400 | 0x800 | 0x1000;

/// Whether the workaround is on: unless `SV_VIDEO_VIEW_CACHE=0`.
pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("SV_VIDEO_VIEW_CACHE").map(|v| v != "0").unwrap_or(true))
}

/// What [`VideoViews::lookup`] says to do with a `vkCreateImageView`.
#[derive(Debug, PartialEq, Eq)]
pub enum Create {
    /// Not a video image: create and destroy as the application says.
    Forward,
    /// This driver view already exists for exactly this description.
    Reuse(u64),
    /// Create it, then [`VideoViews::keep`] it.
    CreateAndKeep,
}

/// The cache, per session's object table.
#[derive(Default)]
pub struct VideoViews {
    /// Driver images created with a video decode usage.
    images: HashSet<u64>,
    /// Per driver image: each description asked for (handles already mapped to
    /// the driver's), and the driver view made for it.
    by_image: HashMap<u64, Vec<(sv_proto::VkImageViewCreateInfo, u64)>>,
    /// Every kept driver view.
    kept: HashSet<u64>,
}

impl VideoViews {
    /// `vkCreateImage` returned `image` for a create info with `usage`.
    pub fn image_created(&mut self, image: u64, usage: u32) {
        if image != 0 && usage & VIDEO_DECODE_USAGE != 0 {
            self.images.insert(image);
        }
    }

    /// What to do with a view of `info.image` (already the driver's handle).
    pub fn lookup(&self, info: &sv_proto::VkImageViewCreateInfo) -> Create {
        let image = info.image.0;
        if !self.images.contains(&image) {
            return Create::Forward;
        }
        match self.by_image.get(&image).and_then(|v| v.iter().find(|(d, _)| d == info)) {
            Some((_, view)) => Create::Reuse(*view),
            None => Create::CreateAndKeep,
        }
    }

    /// The driver made `view` for `info`; keep it until its image goes.
    pub fn keep(&mut self, info: sv_proto::VkImageViewCreateInfo, view: u64) {
        if view == 0 {
            return;
        }
        self.by_image.entry(info.image.0).or_default().push((info, view));
        self.kept.insert(view);
    }

    /// Whether `vkDestroyImageView` of `view` must stay on this side.
    pub fn is_kept(&self, view: u64) -> bool {
        self.kept.contains(&view)
    }

    /// `image` is about to be destroyed: forget it, and return its kept views,
    /// which the caller destroys first.
    pub fn image_destroyed(&mut self, image: u64) -> Vec<u64> {
        self.images.remove(&image);
        let views: Vec<u64> =
            self.by_image.remove(&image).unwrap_or_default().into_iter().map(|(_, v)| v).collect();
        for v in &views {
            self.kept.remove(v);
        }
        views
    }

    /// Every kept view, emptying the cache: for a session's leftovers.
    pub fn take_all(&mut self) -> Vec<u64> {
        self.images.clear();
        self.by_image.clear();
        self.kept.drain().collect()
    }

    /// Kept views, for diagnostics.
    pub fn len(&self) -> usize {
        self.kept.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kept.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(image: u64, base_layer: u32) -> sv_proto::VkImageViewCreateInfo {
        let mut i = sv_proto::VkImageViewCreateInfo::default();
        i.image.0 = image;
        i.subresource_range.base_array_layer = base_layer;
        i.subresource_range.layer_count = 1;
        i
    }

    /// The pattern ffmpeg makes: a view per frame of the same image and
    /// description, destroyed after it. The driver sees one create and no
    /// destroy until the image goes, and then exactly one.
    #[test]
    fn a_video_images_view_is_made_once_and_destroyed_with_the_image() {
        let mut c = VideoViews::default();
        c.image_created(10, 0x1000 | 0x400 | 0x1);
        assert_eq!(c.lookup(&info(10, 0)), Create::CreateAndKeep);
        c.keep(info(10, 0), 100);
        for _ in 0..1000 {
            assert_eq!(c.lookup(&info(10, 0)), Create::Reuse(100));
            assert!(c.is_kept(100), "the application's destroy must not reach the driver");
        }
        assert_eq!(c.image_destroyed(10), [100]);
        assert!(!c.is_kept(100));
        assert!(c.is_empty());
    }

    /// A different description is a different driver view; a non-video image
    /// is not touched at all.
    #[test]
    fn descriptions_are_distinct_and_other_images_are_forwarded() {
        let mut c = VideoViews::default();
        c.image_created(10, 0x1000);
        c.image_created(11, 0x4 | 0x2); // sampled, transfer dst: not video
        c.keep(info(10, 0), 100);
        assert_eq!(c.lookup(&info(10, 1)), Create::CreateAndKeep);
        c.keep(info(10, 1), 101);
        assert_eq!(c.lookup(&info(10, 1)), Create::Reuse(101));
        assert_eq!(c.lookup(&info(11, 0)), Create::Forward);
        let mut gone = c.image_destroyed(10);
        gone.sort();
        assert_eq!(gone, [100, 101]);
    }

    #[test]
    fn leftovers_take_every_kept_view() {
        let mut c = VideoViews::default();
        c.image_created(10, 0x400);
        c.image_created(12, 0x800);
        c.keep(info(10, 0), 100);
        c.keep(info(12, 0), 120);
        let mut all = c.take_all();
        all.sort();
        assert_eq!(all, [100, 120]);
        assert_eq!(c.lookup(&info(10, 0)), Create::Forward);
    }
}
