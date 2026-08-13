//! Where a window's taskbar icon comes from, and what happens when it comes
//! from nowhere.
//!
//! # Two sources, and neither of them is a file
//!
//! 1. **`xdg-toplevel-icon-v1`**, which is the protocol for this and is
//!    implemented by [`crate::state`] through smithay. A client attaches
//!    `wl_buffer`s to an icon object and the compositor reads them. Measured
//!    support in real clients is in docs/design.md §16.2, and the short version
//!    is that GTK 4.20 sends it and Firefox 151 does not.
//! 2. **[`crate::icon::proto`]**, this project's own interface, for everything
//!    else. A resolver on the Linux side turns an app id into pixels and
//!    publishes them here, keyed by app id rather than by object, because it is
//!    a different client from the application and Wayland objects do not cross
//!    clients.
//!
//! Both end in the same place: a set of [`IconImage`]s, and
//! [`ToUi::Icon`](crate::platform::ToUi::Icon).
//!
//! # Why a set rather than an image
//!
//! Windows asks for two sizes and uses them in different places: `ICON_SMALL`
//! is the caption glyph and the Alt-Tab strip at `SM_CXSMICON` (16 px at 96
//! dpi), `ICON_BIG` is the taskbar and the task switcher at `SM_CXICON` (32).
//! An icon theme has both, drawn rather than resampled — hicolor ships 16, 22,
//! 24, 32, 48, 64 and 128 for Firefox on the machine this was written on — so
//! throwing that away at the resolver and scaling one image at the far end
//! would be losing information we were given for free.
//!
//! [`best_match`] is the choice, and it is here rather than in the platform
//! because it is arithmetic and the platform is not testable on the machine
//! that runs `cargo test`.

use std::collections::HashMap;

use crate::platform::IconImage;
use crate::surface::{BufferSpec, PixelFormat};

/// The generated bindings for `protocols/wc-icon-v1.xml`.
///
/// Server side only: nothing in this workspace is a Wayland *client* of this
/// interface. wc-proxy writes the requests by hand out of the same XML file
/// (see its `iconlink` module), which is how the two ends are kept from
/// drifting without a second code generator on the host.
#[allow(missing_docs)]
pub mod proto {
    use smithay::reexports::wayland_server;

    pub mod __interfaces {
        wayland_scanner::generate_interfaces!("protocols/wc-icon-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("protocols/wc-icon-v1.xml");
}

/// The largest icon anything here will hold, per image.
///
/// 256x256 is the biggest size Windows has a use for (`SHIL_JUMBO`), and four
/// bytes a pixel makes 256 KiB. The limit exists because the bytes arrive from
/// a client, and a client that sends chunks and never sends `image` would
/// otherwise be a memory leak with a protocol in front of it.
pub const MAX_IMAGE_BYTES: usize = 256 * 256 * 4;

/// How many images one publish may carry. Three is what the resolver sends
/// (16, 32, 48); eight is room for a theme that has more without being a hole.
pub const MAX_IMAGES: usize = 8;

/// Pick the image to build an `HICON` of `want` x `want` pixels from.
///
/// An exact match wins. Otherwise the smallest image *larger* than the target
/// wins over any image smaller than it, because downscaling an icon loses
/// detail and upscaling invents it — a 48 px source for a 32 px slot is a
/// resample, a 16 px source for the same slot is a blur. Only if everything is
/// smaller does it take the largest of those.
///
/// Non-square images are compared on their larger side, which is what an
/// `HICON` slot constrains.
pub fn best_match(images: &[IconImage], want: i32) -> Option<&IconImage> {
    let side = |i: &IconImage| i.width.max(i.height);
    images
        .iter()
        .filter(|i| side(i) >= want)
        .min_by_key(|i| side(i))
        .or_else(|| images.iter().max_by_key(|i| side(i)))
}

/// Turn one buffer's worth of pixels into a tightly packed BGRA image.
///
/// The stride is the reason this is not a `to_vec`: a `wl_shm` buffer is
/// allowed to have padding at the end of every row, and an icon that ignored it
/// would come out sheared rather than wrong in a way anybody would call a bug.
///
/// `None` for anything that is not a plausible icon — a zero or negative size,
/// a size past [`MAX_IMAGE_BYTES`], or a buffer shorter than the rows it
/// claims. A refusal here degrades to the generic Windows icon, which is the
/// behaviour docs/design.md §16.7 argues for.
pub fn image_from_bytes(bytes: &[u8], spec: BufferSpec) -> Option<IconImage> {
    let (w, h) = (spec.width, spec.height);
    if w <= 0 || h <= 0 {
        return None;
    }
    let bpp = PixelFormat::BYTES_PER_PIXEL;
    let row = (w as usize).checked_mul(bpp)?;
    let total = row.checked_mul(h as usize)?;
    if total > MAX_IMAGE_BYTES {
        return None;
    }
    let stride = usize::try_from(spec.stride).ok()?;
    if stride < row {
        return None;
    }
    let mut pixels = Vec::with_capacity(total);
    for y in 0..h as usize {
        let start = y.checked_mul(stride)?;
        let end = start.checked_add(row)?;
        pixels.extend_from_slice(bytes.get(start..end)?);
    }
    // A window with no alpha channel is opaque; an icon with no alpha channel
    // is a rectangle. `Xrgb8888` says the fourth byte is undefined, and an
    // undefined byte fed to `AlphaBlend` is a hole, so it is made opaque here
    // rather than in the platform — this is the one place that knows the
    // format the bytes arrived in.
    if !spec.format.has_alpha() {
        for px in pixels.chunks_exact_mut(bpp) {
            px[3] = 0xFF;
        }
    }
    Some(IconImage {
        width: w,
        height: h,
        pixels,
    })
}

/// What a publisher has said so far, before it says which app id it meant.
///
/// Split out of the protocol handler so that the arithmetic — which is where
/// a length mistake would be — is testable without a `wl_display`.
#[derive(Debug, Default)]
pub struct Staging {
    bytes: Vec<u8>,
    images: Vec<IconImage>,
}

/// Why a staged image was refused. Each maps to one protocol error.
#[derive(Debug, PartialEq, Eq)]
pub enum StageError {
    /// More bytes staged than [`MAX_IMAGE_BYTES`], or more than [`MAX_IMAGES`]
    /// images in one set.
    TooBig,
    /// `image` named more pixels than have been chunked in.
    Short,
    /// A non-positive or implausible size.
    BadSize,
}

impl Staging {
    pub fn chunk(&mut self, data: &[u8]) -> Result<(), StageError> {
        if self.bytes.len() + data.len() > MAX_IMAGE_BYTES {
            return Err(StageError::TooBig);
        }
        self.bytes.extend_from_slice(data);
        Ok(())
    }

    /// Close off one image, consuming its bytes from the front of the staging
    /// area.
    pub fn image(&mut self, width: i32, height: i32) -> Result<(), StageError> {
        if width <= 0 || height <= 0 || width > 1024 || height > 1024 {
            return Err(StageError::BadSize);
        }
        if self.images.len() >= MAX_IMAGES {
            return Err(StageError::TooBig);
        }
        let need = (width as usize) * (height as usize) * PixelFormat::BYTES_PER_PIXEL;
        if self.bytes.len() < need {
            return Err(StageError::Short);
        }
        let rest = self.bytes.split_off(need);
        let pixels = std::mem::replace(&mut self.bytes, rest);
        self.images.push(IconImage {
            width,
            height,
            pixels,
        });
        Ok(())
    }

    /// Take the set and reset. Trailing chunked bytes that no `image` claimed
    /// are dropped: they are a caller's mistake and holding them would make the
    /// next publish inherit it.
    pub fn take(&mut self) -> Vec<IconImage> {
        self.bytes.clear();
        std::mem::take(&mut self.images)
    }
}

/// Icons published by app id, and the windows they are waiting for.
///
/// The store outlives any one publisher and any one window, which is the point
/// of keying by app id: the resolver may publish before the window exists —
/// it sees `set_app_id` go past on the wire, and the window is not created
/// until the surface first commits — and it may exit afterwards without the
/// window losing its icon.
#[derive(Debug, Default)]
pub struct IconStore {
    by_app_id: HashMap<String, Vec<IconImage>>,
}

impl IconStore {
    /// Install a set. An empty set is a *negative* answer — "this app id has no
    /// icon" — and is kept, so that a second window with the same app id does
    /// not make the resolver look again.
    pub fn publish(&mut self, app_id: String, images: Vec<IconImage>) {
        self.by_app_id.insert(app_id, images);
    }

    pub fn forget(&mut self, app_id: &str) {
        self.by_app_id.remove(app_id);
    }

    /// The set for an app id, or `None` if nobody has published one. An empty
    /// set answers `None` too: to everything downstream, "published as having
    /// no icon" and "unknown" mean the same picture on the taskbar, and the
    /// difference only matters to the resolver.
    pub fn get(&self, app_id: &str) -> Option<&[IconImage]> {
        match self.by_app_id.get(app_id) {
            Some(images) if !images.is_empty() => Some(images),
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        self.by_app_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_app_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: i32, h: i32) -> IconImage {
        IconImage {
            width: w,
            height: h,
            pixels: vec![0u8; (w * h * 4) as usize],
        }
    }

    #[test]
    fn an_exact_size_is_preferred() {
        let set = [img(16, 16), img(32, 32), img(48, 48)];
        assert_eq!(best_match(&set, 32).unwrap().width, 32);
        assert_eq!(best_match(&set, 16).unwrap().width, 16);
    }

    #[test]
    fn scaling_down_beats_scaling_up() {
        // 24 is not in the set. 32 (down by a third) is a better source than 16
        // (up by a half), and 48 is worse than 32 because it is further away.
        let set = [img(16, 16), img(32, 32), img(48, 48)];
        assert_eq!(best_match(&set, 24).unwrap().width, 32);
        // Nothing is big enough: take the biggest there is rather than nothing.
        assert_eq!(best_match(&set, 256).unwrap().width, 48);
        assert!(best_match(&[], 32).is_none());
    }

    #[test]
    fn a_strided_buffer_is_repacked_rather_than_sheared() {
        // Two rows of two pixels, with one pixel of padding at the end of each.
        let spec = BufferSpec {
            width: 2,
            height: 2,
            stride: 12,
            format: PixelFormat::Argb8888,
        };
        let mut bytes = vec![0u8; 24];
        bytes[0..4].copy_from_slice(&[1, 2, 3, 4]);
        bytes[4..8].copy_from_slice(&[5, 6, 7, 8]);
        bytes[12..16].copy_from_slice(&[9, 10, 11, 12]);
        bytes[16..20].copy_from_slice(&[13, 14, 15, 16]);
        let icon = image_from_bytes(&bytes, spec).unwrap();
        assert_eq!(icon.pixels.len(), 16);
        assert_eq!(&icon.pixels[0..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&icon.pixels[8..16], &[9, 10, 11, 12, 13, 14, 15, 16]);
    }

    #[test]
    fn an_opaque_format_gets_an_opaque_alpha_channel() {
        // `Xrgb8888` leaves the fourth byte undefined. Undefined fed to
        // `AlphaBlend` is an invisible icon, which is exactly the failure that
        // looks like "nothing calls WM_SETICON".
        let spec = BufferSpec {
            width: 1,
            height: 1,
            stride: 4,
            format: PixelFormat::Xrgb8888,
        };
        let icon = image_from_bytes(&[9, 9, 9, 0], spec).unwrap();
        assert_eq!(icon.pixels, vec![9, 9, 9, 0xFF]);
    }

    #[test]
    fn implausible_buffers_are_refused_rather_than_trusted() {
        let bad = |w, h, stride, len| {
            image_from_bytes(
                &vec![0u8; len],
                BufferSpec {
                    width: w,
                    height: h,
                    stride,
                    format: PixelFormat::Argb8888,
                },
            )
        };
        assert!(bad(0, 4, 16, 64).is_none(), "zero width");
        assert!(bad(-4, 4, 16, 64).is_none(), "negative width");
        assert!(bad(4, 4, 8, 64).is_none(), "stride under one row");
        assert!(bad(4, 4, 16, 32).is_none(), "buffer shorter than its rows");
        assert!(bad(4096, 4096, 4096 * 4, 16).is_none(), "past the size cap");
    }

    #[test]
    fn staging_splits_one_run_of_chunks_into_several_images() {
        // The resolver is allowed to send every size in one run: this is the
        // arithmetic that lets it, and getting it wrong would give the second
        // image the first one's pixels.
        let mut s = Staging::default();
        s.chunk(&vec![0xAA; 16 * 16 * 4]).unwrap();
        s.chunk(&vec![0xBB; 32 * 32 * 4]).unwrap();
        s.image(16, 16).unwrap();
        s.image(32, 32).unwrap();
        let set = s.take();
        assert_eq!(set.len(), 2);
        assert!(set[0].pixels.iter().all(|&b| b == 0xAA));
        assert!(set[1].pixels.iter().all(|&b| b == 0xBB));
        assert!(s.take().is_empty(), "take resets");
    }

    #[test]
    fn staging_refuses_an_image_bigger_than_its_bytes() {
        let mut s = Staging::default();
        s.chunk(&[0; 16]).unwrap();
        assert_eq!(s.image(32, 32), Err(StageError::Short));
        assert_eq!(s.image(0, 32), Err(StageError::BadSize));
        assert_eq!(s.chunk(&vec![0; MAX_IMAGE_BYTES]), Err(StageError::TooBig));
    }

    #[test]
    fn the_store_answers_none_for_an_app_id_with_no_pictures() {
        let mut store = IconStore::default();
        store.publish("firefox".into(), vec![img(32, 32)]);
        store.publish("weston-terminal".into(), Vec::new());
        assert_eq!(store.get("firefox").unwrap().len(), 1);
        assert!(store.get("weston-terminal").is_none(), "published as having none");
        assert!(store.get("nobody").is_none(), "never published");
        assert_eq!(store.len(), 2, "and the two are still distinguishable here");
        store.forget("firefox");
        assert!(store.get("firefox").is_none());
    }
}
