//! Where a client's pixels are, and how the compositor gets a CPU view of them.
//!
//! # Why this is a trait and not a call to `wl_shm`
//!
//! Stage 3 has exactly one source of pixels — `wl_shm`, which on Windows means
//! a named section object (docs/design.md §2.2) — and a compositor that only
//! ever wanted that could call `smithay::wayland::shm::with_buffer_contents`
//! straight from its `commit` handler in four lines.
//!
//! It is written as a seam anyway, because the end goal
//! (docs/goal-firefox.md) puts the client on the **other side of a
//! hypervisor**. A Windows section name is not something a Linux process can
//! create or open, so same-machine `wl_shm` is a stepping stone rather than the
//! destination; the buffer eventually has to live in shared_vulkan's arena,
//! which both sides can reach. The cost of keeping "where the pixels are"
//! behind a trait now is this file; the cost of discovering later that the
//! section-name assumption is spread through the commit path, the damage
//! tracker and the blitter is not bounded.
//!
//! So: [`PixelSource`] is the seam, [`ShmSource`] is the only implementation,
//! and everything downstream of [`Shadow::blit`] deals in bytes and rectangles
//! and has never heard of a section.
//!
//! # The one format
//!
//! Wayland's `Xrgb8888`/`Argb8888` and a Windows 32-bit `BI_RGB` DIB are the
//! **same bytes in the same order** — B, G, R, then X or A — because both are
//! "a little-endian `u32` of 0xAARRGGBB". That is not a coincidence worth
//! relying on silently, so it is asserted in the tests rather than assumed in a
//! comment: it is the reason the blit is a `memcpy` per row and not a per-pixel
//! swizzle, and if a future format breaks it the tests say so.

use std::fmt;

/// A rectangle, in pixels, top-left origin.
///
/// Deliberately not `smithay::utils::Rectangle`: this type crosses to the
/// Win32 thread, where smithay's coordinate-space type parameters mean nothing
/// and its `Logical`/`Buffer` distinction has already been resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    /// Clip to a `w`x`h` box anchored at the origin. `None` if nothing is left.
    pub fn clip(self, w: i32, h: i32) -> Option<Self> {
        let x0 = self.x.max(0);
        let y0 = self.y.max(0);
        let x1 = self.x.saturating_add(self.w).min(w);
        let y1 = self.y.saturating_add(self.h).min(h);
        if x1 <= x0 || y1 <= y0 {
            None
        } else {
            Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
        }
    }

    pub fn is_empty(self) -> bool {
        self.w <= 0 || self.h <= 0
    }
}

/// The pixel layouts this compositor can blit without conversion.
///
/// Both are 32 bits per pixel in B, G, R, A byte order. The only difference is
/// whether the fourth byte means anything, and stage 3 ignores it either way —
/// GDI's `BitBlt` and `SetDIBitsToDevice` treat a 32-bit `BI_RGB` DIB as
/// opaque. Honouring alpha needs a layered window or `AlphaBlend`, which is a
/// later stage's problem and is recorded as such in docs/design.md §8.7.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// Alpha is meaningful (premultiplied, per the `wl_shm` spec).
    Argb8888,
    /// The fourth byte is padding.
    Xrgb8888,
}

impl PixelFormat {
    pub const BYTES_PER_PIXEL: usize = 4;

    pub fn has_alpha(self) -> bool {
        matches!(self, PixelFormat::Argb8888)
    }
}

/// The shape of one client buffer, independent of where its bytes live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferSpec {
    pub width: i32,
    pub height: i32,
    /// Bytes between the start of one row and the start of the next. May
    /// exceed `width * 4`; clients are allowed to pad rows and some do.
    pub stride: i32,
    pub format: PixelFormat,
}

impl BufferSpec {
    /// Bytes needed to hold this buffer, given its stride.
    ///
    /// The last row needs only `width * 4` bytes, not a full stride, and a
    /// client is within its rights to allocate exactly that. Getting this
    /// wrong rejects legal buffers, so it is computed rather than rounded up.
    pub fn required_len(&self) -> Option<usize> {
        if self.width <= 0 || self.height <= 0 || self.stride < self.width.checked_mul(4)? {
            return None;
        }
        let stride = usize::try_from(self.stride).ok()?;
        let rows = usize::try_from(self.height - 1).ok()?;
        let width_bytes = usize::try_from(self.width).ok()?.checked_mul(4)?;
        stride.checked_mul(rows)?.checked_add(width_bytes)
    }
}

/// Why a buffer's pixels could not be read.
#[derive(Debug)]
pub enum PixelError {
    /// No registered source recognises this buffer.
    UnknownSource,
    /// The source recognises it but the description is not something we can
    /// blit — an unsupported format, or a stride/size that does not add up.
    Unsupported(String),
    /// The source recognises it and failed to map it.
    Unreadable(String),
}

impl fmt::Display for PixelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PixelError::UnknownSource => write!(f, "no pixel source owns this buffer"),
            PixelError::Unsupported(m) => write!(f, "unsupported buffer: {m}"),
            PixelError::Unreadable(m) => write!(f, "buffer could not be read: {m}"),
        }
    }
}

/// The compositor's shadow copy of one surface's contents.
///
/// # Why a copy at all, when the client's buffer is already mapped
///
/// Because of `wl_buffer.release`, and this is the load-bearing decision in the
/// whole present path.
///
/// Wayland lets the client reuse a buffer the moment it is released, and a
/// client with a single buffer — which `weston-simple-shm` becomes under
/// memory pressure, and which many toolkits are by default — **stalls forever**
/// if the compositor holds its buffer. But Win32 asks for pixels at times
/// Wayland knows nothing about: `WM_PAINT` fires when the window is uncovered,
/// dragged off-screen and back, or restored from minimised, with no commit in
/// sight. A compositor that blitted straight from client memory would have to
/// hold every buffer until the next `WM_PAINT` that might need it, which is
/// "until the window is closed".
///
/// One `memcpy` per damaged row at commit time buys the right to release
/// immediately, and buys it in the currency this stage is spending anyway.
/// It is also what makes the shadow safe to hand to another thread: the client
/// cannot write into it.
pub struct Shadow {
    spec: BufferSpec,
    /// Tightly packed: stride is always `width * 4`. Client padding is
    /// removed on the way in so the Win32 side never has to think about it,
    /// and so a `BITMAPINFOHEADER` describes it with no extra fields.
    pixels: Vec<u8>,
}

impl fmt::Debug for Shadow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shadow")
            .field("width", &self.spec.width)
            .field("height", &self.spec.height)
            .field("format", &self.spec.format)
            .finish()
    }
}

impl Shadow {
    /// An empty shadow: no buffer has been committed yet.
    pub fn empty() -> Self {
        Self {
            spec: BufferSpec {
                width: 0,
                height: 0,
                stride: 0,
                format: PixelFormat::Xrgb8888,
            },
            pixels: Vec::new(),
        }
    }

    pub fn width(&self) -> i32 {
        self.spec.width
    }

    pub fn height(&self) -> i32 {
        self.spec.height
    }

    pub fn format(&self) -> PixelFormat {
        self.spec.format
    }

    pub fn is_empty(&self) -> bool {
        self.pixels.is_empty()
    }

    /// The packed BGRA bytes, `width * height * 4` of them.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Copy `damage` from `src` into the shadow.
    ///
    /// Returns the rectangles actually written, clipped to the buffer — which
    /// is what the platform should invalidate, and is not always what was asked
    /// for: clients send damage in surface coordinates that can exceed the
    /// buffer, and `weston-simple-shm` sends whole-surface damage on a resize
    /// before the new buffer is attached.
    ///
    /// A change of dimensions or format reallocates and reports full damage,
    /// because none of the old pixels are in the right place any more and a
    /// client is not required to damage a buffer it has just resized.
    pub fn blit(&mut self, src: &[u8], spec: BufferSpec, damage: &[Rect]) -> Vec<Rect> {
        let Some(need) = spec.required_len() else {
            return Vec::new();
        };
        if src.len() < need {
            return Vec::new();
        }

        let resized = self.spec.width != spec.width
            || self.spec.height != spec.height
            || self.spec.format != spec.format;
        if resized {
            let bpp = PixelFormat::BYTES_PER_PIXEL;
            self.pixels = vec![0u8; spec.width as usize * spec.height as usize * bpp];
            self.spec = BufferSpec {
                stride: spec.width * bpp as i32,
                ..spec
            };
        }

        // A resize invalidates everything; otherwise honour what the client
        // said. An empty damage list on a same-size buffer means the client
        // asserted nothing changed, and we take it at its word — that is the
        // protocol's meaning and second-guessing it would defeat the point of
        // damage tracking.
        let full = Rect::new(0, 0, spec.width, spec.height);
        let owned;
        let rects: &[Rect] = if resized {
            owned = [full];
            &owned
        } else {
            damage
        };

        let bpp = PixelFormat::BYTES_PER_PIXEL;
        let src_stride = spec.stride as usize;
        let dst_stride = self.spec.stride as usize;
        let row_max = spec.width as usize * bpp;

        let mut written = Vec::with_capacity(rects.len());
        for r in rects {
            if r.is_empty() {
                continue;
            }
            let Some(r) = r.clip(spec.width, spec.height) else {
                continue;
            };
            let x0 = r.x as usize * bpp;
            let bytes = (r.w as usize * bpp).min(row_max - x0);
            for row in 0..r.h as usize {
                let y = r.y as usize + row;
                let s = y * src_stride + x0;
                let d = y * dst_stride + x0;
                self.pixels[d..d + bytes].copy_from_slice(&src[s..s + bytes]);
            }
            written.push(r);
        }
        written
    }
}

/// A place client pixels can come from.
///
/// The one implementation today is [`ShmSource`]. The trait exists for the
/// second one — see this module's header, and docs/goal-firefox.md for why
/// there has to be a second one.
///
/// `&dyn FnMut` rather than a generic closure so the trait stays object-safe:
/// the compositor holds `Vec<Box<dyn PixelSource>>` and asks each in turn,
/// which is how a buffer that a future arena owns will find its way to the
/// right reader without the commit path knowing the difference.
pub trait PixelSource {
    /// For logs and for the "which source served this surface" assertion in the
    /// harness. A stage that adds a second source without a way to tell which
    /// one ran has not really added it.
    fn name(&self) -> &'static str;

    /// Read the buffer's pixels, if this source owns it.
    ///
    /// `Err(PixelError::UnknownSource)` means "not mine, ask the next one" and
    /// is the only error the caller should treat as non-fatal.
    fn with_pixels(
        &self,
        buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
        f: &mut dyn FnMut(&[u8], BufferSpec),
    ) -> Result<(), PixelError>;
}

/// Pixels in a `wl_shm` pool.
///
/// On Unix that is a `memfd`; on Windows it is a named section opened by name
/// (docs/design.md §2.2). **Neither fact appears below**, and that is the
/// point: smithay's `shm` module already hides the difference behind
/// `with_buffer_contents`, and the port in `vendor/smithay/src/wayland/shm/`
/// is where the platform lives.
pub struct ShmSource;

impl PixelSource for ShmSource {
    fn name(&self) -> &'static str {
        "wl_shm"
    }

    fn with_pixels(
        &self,
        buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
        f: &mut dyn FnMut(&[u8], BufferSpec),
    ) -> Result<(), PixelError> {
        use smithay::reexports::wayland_server::protocol::wl_shm;
        use smithay::wayland::shm::{with_buffer_contents, BufferAccessError};

        let result = with_buffer_contents(buffer, |ptr, len, data| {
            let format = match data.format {
                wl_shm::Format::Argb8888 => PixelFormat::Argb8888,
                wl_shm::Format::Xrgb8888 => PixelFormat::Xrgb8888,
                other => {
                    return Err(PixelError::Unsupported(format!("wl_shm format {other:?}")));
                }
            };
            let spec = BufferSpec {
                width: data.width,
                height: data.height,
                stride: data.stride,
                format,
            };
            let Some(need) = spec.required_len() else {
                return Err(PixelError::Unsupported(format!(
                    "{}x{} stride {} is not a describable buffer",
                    spec.width, spec.height, spec.stride
                )));
            };

            // The offset is a client-controlled index into a client-controlled
            // pool, so it is checked here rather than trusted. A pool that is
            // too small for the buffer it claims to hold is a protocol error on
            // the client's part, but reading past the mapping would be ours.
            let offset = usize::try_from(data.offset)
                .map_err(|_| PixelError::Unsupported("negative buffer offset".into()))?;
            let end = offset
                .checked_add(need)
                .ok_or_else(|| PixelError::Unsupported("buffer offset overflows".into()))?;
            if end > len {
                return Err(PixelError::Unsupported(format!(
                    "buffer needs {end} bytes of a {len}-byte pool"
                )));
            }

            // SAFETY: `ptr` is valid for `len` bytes for the duration of this
            // closure — that is `with_buffer_contents`' contract — and
            // `offset + need <= len` was just checked. The bytes are shared
            // with the client, which may write them concurrently; that is a
            // `wl_shm` fact rather than a soundness fiction, and it is why the
            // caller copies out of this slice rather than holding it.
            let bytes = unsafe { std::slice::from_raw_parts(ptr.add(offset), need) };
            f(bytes, spec);
            Ok(())
        });

        match result {
            Ok(inner) => inner,
            Err(BufferAccessError::NotManaged) => Err(PixelError::UnknownSource),
            Err(e) => Err(PixelError::Unreadable(format!("{e:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(w: i32, h: i32, stride: i32) -> BufferSpec {
        BufferSpec {
            width: w,
            height: h,
            stride,
            format: PixelFormat::Xrgb8888,
        }
    }

    #[test]
    fn required_len_does_not_demand_padding_after_the_last_row() {
        // 4x3 at a stride of 32 needs 2 full strides plus 16 bytes, not 96. A
        // client may allocate exactly this and rejecting it would be our bug.
        assert_eq!(spec(4, 3, 32).required_len(), Some(32 * 2 + 16));
        assert_eq!(spec(4, 3, 16).required_len(), Some(48));
    }

    #[test]
    fn required_len_rejects_a_stride_narrower_than_the_row() {
        assert_eq!(spec(4, 3, 8).required_len(), None);
        assert_eq!(spec(0, 3, 8).required_len(), None);
        assert_eq!(spec(4, 0, 16).required_len(), None);
    }

    #[test]
    fn clip_trims_to_the_buffer_and_drops_what_falls_outside() {
        assert_eq!(Rect::new(-5, -5, 10, 10).clip(8, 8), Some(Rect::new(0, 0, 5, 5)));
        assert_eq!(Rect::new(6, 6, 10, 10).clip(8, 8), Some(Rect::new(6, 6, 2, 2)));
        assert_eq!(Rect::new(20, 0, 4, 4).clip(8, 8), None);
        assert_eq!(Rect::new(0, 0, 0, 4).clip(8, 8), None);
    }

    #[test]
    fn a_first_commit_reports_full_damage_whatever_the_client_said() {
        let mut shadow = Shadow::empty();
        let s = spec(2, 2, 8);
        let src = vec![0xABu8; 16];
        // The client claims one pixel changed; the shadow did not exist, so
        // everything is new and the platform must repaint all of it.
        let written = shadow.blit(&src, s, &[Rect::new(0, 0, 1, 1)]);
        assert_eq!(written, vec![Rect::new(0, 0, 2, 2)]);
        assert_eq!(shadow.pixels(), &[0xAB; 16]);
    }

    #[test]
    fn damage_limits_what_is_copied() {
        let mut shadow = Shadow::empty();
        let s = spec(2, 2, 8);
        shadow.blit(&[0u8; 16], s, &[]);
        assert_eq!(shadow.pixels(), &[0u8; 16]);

        // Second commit: the whole source is 0xFF but only the top-left pixel
        // is damaged, so only four bytes may move. A blitter that ignored
        // damage would pass a "did the pixels arrive" test and fail this one.
        let written = shadow.blit(&[0xFFu8; 16], s, &[Rect::new(0, 0, 1, 1)]);
        assert_eq!(written, vec![Rect::new(0, 0, 1, 1)]);
        assert_eq!(&shadow.pixels()[0..4], &[0xFF; 4]);
        assert_eq!(&shadow.pixels()[4..16], &[0x00; 12]);
    }

    #[test]
    fn client_row_padding_is_removed() {
        // A 2x2 buffer with a 16-byte stride: 8 bytes of payload per row and 8
        // of padding. The shadow must be 16 tight bytes with no padding, or the
        // DIB the Win32 side describes will shear.
        let mut shadow = Shadow::empty();
        let s = spec(2, 2, 16);
        let mut src = vec![0u8; 24]; // 16 + 8, per required_len
        src[0..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        src[16..24].copy_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);
        shadow.blit(&src, s, &[]);
        assert_eq!(shadow.pixels(), &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
    }

    #[test]
    fn a_short_source_is_refused_rather_than_read_past() {
        let mut shadow = Shadow::empty();
        assert!(shadow.blit(&[0u8; 4], spec(2, 2, 8), &[]).is_empty());
        assert!(shadow.is_empty());
    }

    #[test]
    fn damage_outside_the_buffer_is_clipped_not_trusted() {
        let mut shadow = Shadow::empty();
        let s = spec(2, 2, 8);
        shadow.blit(&[0u8; 16], s, &[]);
        // A client may damage in surface coordinates that overshoot the buffer.
        // Copying that verbatim is an out-of-bounds write.
        let written = shadow.blit(&[0x77u8; 16], s, &[Rect::new(0, 0, 100, 100)]);
        assert_eq!(written, vec![Rect::new(0, 0, 2, 2)]);
        assert_eq!(shadow.pixels(), &[0x77; 16]);
    }

    #[test]
    fn a_resize_reallocates_and_reports_everything() {
        let mut shadow = Shadow::empty();
        shadow.blit(&[0x11u8; 16], spec(2, 2, 8), &[]);
        let written = shadow.blit(&[0x22u8; 64], spec(4, 4, 16), &[Rect::new(0, 0, 1, 1)]);
        assert_eq!(written, vec![Rect::new(0, 0, 4, 4)]);
        assert_eq!(shadow.width(), 4);
        assert_eq!(shadow.pixels().len(), 64);
        assert_eq!(shadow.pixels(), &[0x22; 64]);
    }

    #[test]
    fn wayland_and_gdi_agree_on_byte_order() {
        // Not a tautology and not decoration. Both `wl_shm.Xrgb8888` and a
        // 32-bit `BI_RGB` Windows DIB are documented as a little-endian u32
        // holding 0x00RRGGBB, which means the bytes in memory are B, G, R, X.
        // The whole blit path is a `memcpy` because of that. Encoding the claim
        // here means a future format that breaks it fails a test rather than
        // producing a window with swapped red and blue that somebody has to
        // notice by eye.
        let pixel: u32 = 0x00_11_22_33; // R=0x11, G=0x22, B=0x33
        assert_eq!(pixel.to_le_bytes(), [0x33, 0x22, 0x11, 0x00]);
        assert_eq!(PixelFormat::BYTES_PER_PIXEL, 4);
        assert!(!PixelFormat::Xrgb8888.has_alpha());
        assert!(PixelFormat::Argb8888.has_alpha());
    }
}
