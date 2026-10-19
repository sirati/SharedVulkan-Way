//! Where a client's pixels are, and how the compositor gets a CPU view of them.
//!
//! # Why this is a trait and not a call to `wl_shm`
//!
//! Stage 3 has exactly one source of pixels — `wl_shm`, which on Windows means
//! a named section object (docs/waycompose-design.md §2.2) — and a compositor that only
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

/// A clip that clips nothing, for the tests' calls to [`Shadow::compose`].
#[cfg(test)]
const EVERYWHERE: Rect = Rect { x: 0, y: 0, w: i32::MAX, h: i32::MAX };

/// The pixel layouts this compositor can blit without conversion.
///
/// Both are 32 bits per pixel in B, G, R, A byte order. The only difference is
/// whether the fourth byte means anything, and stage 3 ignores it either way —
/// GDI's `BitBlt` and `SetDIBitsToDevice` treat a 32-bit `BI_RGB` DIB as
/// opaque. Honouring alpha needs a layered window or `AlphaBlend`, which is a
/// later stage's problem and is recorded as such in docs/waycompose-design.md §8.7.
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

    /// Make this shadow `w`x`h` of `format`, filled with transparent black,
    /// reallocating only when the shape actually changes.
    ///
    /// ADDED FOR STAGE 9, for the scene graph. A window's shadow is no longer a
    /// copy of one client buffer — it is the surface **tree** composited
    /// together — so something has to decide how big the canvas is before any
    /// of the pieces are drawn into it. Returns whether it reallocated, which
    /// is the caller's cue that every pixel is damaged.
    pub fn reset(&mut self, w: i32, h: i32, format: PixelFormat) -> bool {
        let bpp = PixelFormat::BYTES_PER_PIXEL;
        if self.spec.width == w && self.spec.height == h && self.spec.format == format {
            return false;
        }
        self.spec = BufferSpec {
            width: w,
            height: h,
            stride: w.saturating_mul(bpp as i32),
            format,
        };
        self.pixels = vec![0u8; (w.max(0) as usize) * (h.max(0) as usize) * bpp];
        true
    }

    /// Where `src` placed at `at` lands on this shadow, inside `within`.
    fn landing(&self, src: &Shadow, at: (i32, i32), within: Rect) -> Option<Rect> {
        if src.is_empty() || self.is_empty() {
            return None;
        }
        let r = Rect::new(at.0, at.1, src.width(), src.height()).clip(self.spec.width, self.spec.height)?;
        let x0 = r.x.max(within.x);
        let y0 = r.y.max(within.y);
        let x1 = (r.x + r.w).min(within.x.saturating_add(within.w));
        let y1 = (r.y + r.h).min(within.y.saturating_add(within.h));
        (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
    }

    /// Whether `src` placed at `at` covers all of `r`.
    pub fn covers(src: &Shadow, at: (i32, i32), r: Rect) -> bool {
        !src.is_empty()
            && at.0 <= r.x
            && at.1 <= r.y
            && at.0 + src.width() >= r.x + r.w
            && at.1 + src.height() >= r.y + r.h
    }

    /// Copy `src` into this shadow with its top-left corner at `at`, clipped to
    /// both.
    ///
    /// ADDED FOR STAGE 9. **This is a copy and not a blend, and that is a
    /// decision rather than an omission.** Wayland's `wl_shm` alpha is
    /// premultiplied, so a correct sibling blend is one multiply-add per
    /// channel per pixel and the surfaces that need it are the ones where a
    /// client has told us, through `wl_surface.set_opaque_region`, that it does
    /// not. Firefox's own tree is a transparent GTK shadow surface with one
    /// opaque rectangular subsurface on top of it (docs/waycompose-design.md §13.5), for
    /// which copy and blend give identical pixels. When something arrives whose
    /// subsurfaces really are translucent — a rounded corner over a background
    /// this compositor drew — the difference will show as a hard edge, and that
    /// is the point at which this becomes a blend.
    ///
    /// §95: that point came (Firefox's `<select>` dropdown). The root of a tree
    /// is still copied with this; every surface drawn after it goes through
    /// [`Shadow::compose_over`].
    ///
    /// §104's follow-up: only the pixels inside `within` are written, which is
    /// how the damage-scoped composite (`State::compose_tree`, through
    /// [`Shadow::compose_stack`]) recomposes the 64x64 pixels a frame changed
    /// rather than the 1280x720 window.
    pub fn compose(&mut self, src: &Shadow, at: (i32, i32), within: Rect) {
        let bpp = PixelFormat::BYTES_PER_PIXEL;
        let Some(dst) = self.landing(src, at, within) else {
            return;
        };
        // Where in the source the clipped destination starts: non-zero exactly
        // when the child hangs off the top or left of the canvas, which is the
        // normal case for a GTK client whose window geometry crops its own
        // shadow margin away.
        let sx = dst.x - at.0;
        let sy = dst.y - at.1;
        let src_stride = src.spec.stride as usize;
        let dst_stride = self.spec.stride as usize;
        let bytes = dst.w as usize * bpp;
        for row in 0..dst.h as usize {
            let s = (sy as usize + row) * src_stride + sx as usize * bpp;
            let d = (dst.y as usize + row) * dst_stride + dst.x as usize * bpp;
            self.pixels[d..d + bytes].copy_from_slice(&src.pixels[s..s + bytes]);
        }
    }

    /// Draw `src` **over** this shadow with its top-left corner at `at`,
    /// clipped to both: `dst = src + dst * (1 - src.alpha)`, which is the
    /// Porter-Duff "over" for the premultiplied pixels `wl_shm`'s ARGB is.
    ///
    /// §95. [`Shadow::compose`]'s note said this would be needed "when something
    /// arrives whose subsurfaces really are translucent", and Firefox's `<select>`
    /// dropdown is that: GTK makes it a `wl_subsurface` of the window (not an
    /// `xdg_popup`), drawn with a transparent shadow margin and rounded corners.
    /// Copied, the margin's (0,0,0,0) pixels replaced the page under them and
    /// the dropdown wore a thick black frame, on the guest and in the Linux
    /// build's frame dumps alike.
    ///
    /// An `Xrgb8888` source is copied: its fourth byte is padding, not alpha.
    /// A row whose pixels are all opaque is copied too, which is every row of
    /// Firefox's content subsurface, so the common frame costs a scan of the
    /// alpha bytes on top of the `memcpy` it always cost.
    ///
    /// Only the pixels inside `within` are written, as for [`Shadow::compose`].
    pub fn compose_over(&mut self, src: &Shadow, at: (i32, i32), within: Rect) {
        if !src.spec.format.has_alpha() {
            self.compose(src, at, within);
            return;
        }
        let bpp = PixelFormat::BYTES_PER_PIXEL;
        let Some(dst) = self.landing(src, at, within) else {
            return;
        };
        let sx = dst.x - at.0;
        let sy = dst.y - at.1;
        let src_stride = src.spec.stride as usize;
        let dst_stride = self.spec.stride as usize;
        let bytes = dst.w as usize * bpp;
        for row in 0..dst.h as usize {
            let s = (sy as usize + row) * src_stride + sx as usize * bpp;
            let d = (dst.y as usize + row) * dst_stride + dst.x as usize * bpp;
            let from = &src.pixels[s..s + bytes];
            let to = &mut self.pixels[d..d + bytes];
            if from.chunks_exact(bpp).all(|p| p[3] == 0xFF) {
                to.copy_from_slice(from);
                continue;
            }
            for (o, i) in to.chunks_exact_mut(bpp).zip(from.chunks_exact(bpp)) {
                match i[3] {
                    0xFF => o.copy_from_slice(i),
                    0 => {}
                    a => {
                        let keep = 255 - u32::from(a);
                        for c in 0..4 {
                            // Rounded division by 255; premultiplied, so a
                            // channel never exceeds its alpha and the sum
                            // never exceeds 255.
                            let under = (u32::from(o[c]) * keep + 127) / 255;
                            o[c] = (u32::from(i[c]) + under).min(255) as u8;
                        }
                    }
                }
            }
        }
    }

    /// Recompose the part `r` of this canvas from a stack of surfaces, bottom
    /// to top, each with where its top-left corner lands on the canvas; `None`
    /// is a surface of the tree that has no picture.
    ///
    /// ADDED FOR §104's follow-up, as the one place a window's composite is
    /// computed, for the whole canvas and for a damaged part of it alike
    /// (`State::compose_tree`). Every pixel of `r` is recomputed from the
    /// stack alone: the first surface is copied, and a part of `r` it does not
    /// cover is cleared first, so recomposing a pixel twice — two damage
    /// rectangles that overlap — gives what recomposing it once gives, and a
    /// part gives exactly what the whole would have given there.
    pub fn compose_stack(&mut self, layers: &[(Option<&Shadow>, (i32, i32))], r: Rect) {
        let mut first = true;
        for (src, at) in layers {
            match src {
                Some(src) if first => {
                    if !Shadow::covers(src, *at, r) {
                        self.clear_rect(r);
                    }
                    self.compose(src, *at, r);
                }
                Some(src) => self.compose_over(src, *at, r),
                None if first => self.clear_rect(r),
                None => {}
            }
            first = false;
        }
        if first {
            self.clear_rect(r);
        }
    }

    /// The pixels inside `r` transparent black.
    pub fn clear_rect(&mut self, r: Rect) {
        let Some(r) = r.clip(self.spec.width, self.spec.height) else { return };
        let bpp = PixelFormat::BYTES_PER_PIXEL;
        let stride = self.spec.stride as usize;
        for y in r.y as usize..(r.y + r.h) as usize {
            let d = y * stride + r.x as usize * bpp;
            self.pixels[d..d + r.w as usize * bpp].fill(0);
        }
    }
}

/// Write a shadow out as a binary PPM.
///
/// # Why this is here rather than in one platform
///
/// It was in `platform/headless.rs` until stage 4, and being there was the
/// reason docs/waycompose-design.md §8.5's evidence had to be split: the Linux runs could
/// read their pixels back with `identify` and the Wine runs could not, so the
/// half that produced an `HWND` was also the half whose pixels nobody outside
/// the process ever measured. A `Shadow` is platform-neutral — it is packed
/// BGRA and a width and a height — so there was never a reason for the reader
/// to be.
///
/// PPM rather than PNG because it needs no encoder, and `P6` because a text
/// PPM of a 4 MiB frame is 12 MiB of ASCII. ImageMagick reads both.
pub fn write_ppm(path: &std::path::Path, shadow: &Shadow) -> std::io::Result<()> {
    use std::io::Write as _;
    let (w, h) = (shadow.width(), shadow.height());
    if w <= 0 || h <= 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "nothing committed yet",
        ));
    }
    let src = shadow.pixels();
    let mut out = Vec::with_capacity(16 + src.len() / 4 * 3);
    out.extend_from_slice(format!("P6\n{w} {h}\n255\n").as_bytes());
    for px in src.chunks_exact(PixelFormat::BYTES_PER_PIXEL) {
        // B, G, R in memory; R, G, B in the file. The swap is the whole of the
        // difference between a Wayland `Xrgb8888` buffer and a PPM, and getting
        // it backwards would make every harness's colour assertion measure the
        // reader rather than the compositor.
        out.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    std::fs::File::create(path)?.write_all(&out)
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
/// (docs/waycompose-design.md §2.2). **Neither fact appears below**, and that is the
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

// ---------------------------------------------------------------------------
// §102: single-pixel buffers and viewports, for a nested compositor
// ---------------------------------------------------------------------------

/// A `wp_single_pixel_buffer_manager_v1` buffer: one pixel of one colour.
///
/// ADDED FOR §102. KWin's Wayland backend gives its window's main surface
/// exactly one of these — opaque black, stretched by a viewport to the
/// output's size — and puts the picture in subsurfaces above it. It is a
/// [`PixelSource`] rather than a special case in the commit path because that
/// is what the seam was made for: the pixel is "read" like any other buffer,
/// and the viewport, not this source, makes it the size of the window.
///
/// The protocol's channels are premultiplied and span the whole `u32` range;
/// the byte is the nearest 8-bit value (smithay's `rgba8888`). An opaque colour
/// becomes `Xrgb8888`, so a window whose root is one is no more "translucent"
/// to the platform than a client that drew the same black into `wl_shm`.
pub struct SinglePixelSource;

impl PixelSource for SinglePixelSource {
    fn name(&self) -> &'static str {
        "wp_single_pixel_buffer"
    }

    fn with_pixels(
        &self,
        buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
        f: &mut dyn FnMut(&[u8], BufferSpec),
    ) -> Result<(), PixelError> {
        let Ok(data) = smithay::wayland::single_pixel_buffer::get_single_pixel_buffer(buffer)
        else {
            return Err(PixelError::UnknownSource);
        };
        let [r, g, b, a] = data.rgba8888();
        let format = if data.has_alpha() { PixelFormat::Argb8888 } else { PixelFormat::Xrgb8888 };
        f(&single_pixel_bgra([r, g, b, a]), BufferSpec { width: 1, height: 1, stride: 4, format });
        Ok(())
    }
}

/// The one pixel in memory order, which is B, G, R, A (this module's header).
fn single_pixel_bgra([r, g, b, a]: [u8; 4]) -> [u8; 4] {
    [b, g, r, a]
}

/// How a surface's buffer becomes its picture, when that is not one buffer
/// pixel per surface pixel.
///
/// ADDED FOR §102, for `wp_viewporter`. `src` is the part of the buffer that is
/// shown, **in buffer pixels** (the protocol's surface-local source rectangle
/// multiplied by the buffer scale); `dst` is the surface's size, which is what
/// the surface *is* to everything else in this compositor — window sizing,
/// subsurface layout, the hit test and damage all read the staging shadow, and
/// for a viewported surface that shadow is `dst` big.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub src: (f64, f64, f64, f64),
    pub dst: (i32, i32),
}

/// The view a surface's committed viewport asks for, or `None` when its
/// buffer is to be drawn as it always was.
///
/// `src` and `dst` are the viewport's double-buffered state as smithay keeps it
/// (`ViewportCachedState`), `scale` the surface's buffer scale.
///
/// **No viewport is `None` whatever the scale**, and that is deliberate: this
/// compositor has always drawn a buffer of scale ≠ 1 unscaled (and said so in
/// the log), every client measured before §102 is on that path, and §102 is
/// not the place to change what they see. A viewport that comes out as one
/// buffer pixel per surface pixel is `None` too — KWin's primary layer is
/// exactly that, a 1280x800 source shown at 1280x800 — so the common case
/// costs nothing beyond what an ordinary `wl_shm` surface costs.
pub fn view_of(
    buffer: (i32, i32),
    scale: i32,
    src: Option<(f64, f64, f64, f64)>,
    dst: Option<(i32, i32)>,
) -> Option<View> {
    if src.is_none() && dst.is_none() {
        return None;
    }
    let (bw, bh) = buffer;
    if bw <= 0 || bh <= 0 {
        return None;
    }
    let s = f64::from(scale.max(1));
    let src_buf = match src {
        Some((x, y, w, h)) => (x * s, y * s, w * s, h * s),
        None => (0.0, 0.0, f64::from(bw), f64::from(bh)),
    };
    let dst = match (dst, src) {
        (Some(d), _) => d,
        // The protocol: with no destination the surface is the source
        // rectangle's size, which must then be whole (smithay raises
        // `bad_size` before the state is applied if it is not).
        (None, Some((_, _, w, h))) => (w as i32, h as i32),
        (None, None) => unreachable!("returned above"),
    };
    if dst.0 <= 0 || dst.1 <= 0 || src_buf.2 <= 0.0 || src_buf.3 <= 0.0 {
        return None;
    }
    let whole = (0.0, 0.0, f64::from(bw), f64::from(bh));
    if src_buf == whole && dst == (bw, bh) {
        return None;
    }
    Some(View { src: src_buf, dst })
}

impl View {
    /// Where buffer-pixel damage lands on the surface: each rectangle scaled
    /// by the view, widened by a pixel each way so that rounding can never
    /// leave a stale edge, and clipped to the surface.
    pub fn map_damage(&self, damage: &[Rect]) -> Vec<Rect> {
        let (sx, sy, sw, sh) = self.src;
        let (dw, dh) = self.dst;
        let kx = f64::from(dw) / sw;
        let ky = f64::from(dh) / sh;
        damage
            .iter()
            .filter_map(|r| {
                let x0 = ((f64::from(r.x) - sx) * kx).floor() as i32 - 1;
                let y0 = ((f64::from(r.y) - sy) * ky).floor() as i32 - 1;
                let x1 = ((f64::from(r.x + r.w) - sx) * kx).ceil() as i32 + 1;
                let y1 = ((f64::from(r.y + r.h) - sy) * ky).ceil() as i32 + 1;
                Rect::new(x0, y0, x1 - x0, y1 - y0).clip(dw, dh)
            })
            .collect()
    }
}

impl Shadow {
    /// Draw `src` into this shadow through `view`: the view's source rectangle
    /// of `src`, scaled to the view's destination size, which this shadow
    /// becomes.
    ///
    /// ADDED FOR §102. **Nearest-neighbour**, sampling each destination pixel's
    /// centre. Correctness first: the protocol leaves the filter to the
    /// compositor, the two cases a nested KWin produces are a 1x1 colour (which
    /// every filter draws identically) and a layer whose source and destination
    /// match (which never gets here, see [`view_of`]), and a bilinear filter on
    /// premultiplied pixels is a renderer's worth of code to be right about.
    ///
    /// Only `rects` (in destination pixels) are redrawn, unless the shadow
    /// changes shape, when everything is; returns what was written, which is
    /// what the caller damages.
    pub fn scale_from(&mut self, src: &Shadow, view: &View, rects: Option<&[Rect]>) -> Vec<Rect> {
        let (dw, dh) = view.dst;
        if src.is_empty() || dw <= 0 || dh <= 0 {
            return Vec::new();
        }
        let full = Rect::new(0, 0, dw, dh);
        let reshaped = self.reset(dw, dh, src.format());
        let owned;
        let rects: &[Rect] = match rects {
            Some(r) if !reshaped => r,
            _ => {
                owned = [full];
                &owned
            }
        };
        let bpp = PixelFormat::BYTES_PER_PIXEL;
        let (sw, sh) = (src.width(), src.height());
        let (vx, vy, vw, vh) = view.src;
        // One source column per destination column and one source row per
        // destination row, worked out once rather than per pixel.
        let sample = |d: i32, n: i32, origin: f64, len: f64, limit: i32| -> usize {
            let at = origin + (f64::from(d) + 0.5) * len / f64::from(n);
            (at.floor() as i32).clamp(0, limit - 1) as usize
        };
        let cols: Vec<usize> = (0..dw).map(|x| sample(x, dw, vx, vw, sw)).collect();
        let dst_stride = self.spec.stride as usize;
        let src_stride = src.spec.stride as usize;
        let mut written = Vec::with_capacity(rects.len());
        for r in rects {
            let Some(r) = r.clip(dw, dh) else { continue };
            for y in r.y..r.y + r.h {
                let sy = sample(y, dh, vy, vh, sh);
                let row = &src.pixels[sy * src_stride..sy * src_stride + sw as usize * bpp];
                let d0 = y as usize * dst_stride;
                for x in r.x..r.x + r.w {
                    let s = cols[x as usize] * bpp;
                    let d = d0 + x as usize * bpp;
                    self.pixels[d..d + bpp].copy_from_slice(&row[s..s + bpp]);
                }
            }
            written.push(r);
        }
        written
    }
}

#[cfg(test)]
mod view_tests {
    use super::*;

    fn filled(w: i32, h: i32, format: PixelFormat, f: impl Fn(i32, i32) -> [u8; 4]) -> Shadow {
        let mut s = Shadow::empty();
        let mut px = Vec::new();
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&f(x, y));
            }
        }
        s.blit(&px, BufferSpec { width: w, height: h, stride: w * 4, format }, &[]);
        s
    }

    /// KWin's main surface: a 1x1 opaque black buffer with a destination of
    /// the output's size. Every pixel of the surface is that black, and the
    /// surface is the destination's size — which is what the window is sized by.
    #[test]
    fn a_single_pixel_stretched_by_a_viewport_fills_the_destination() {
        let one = filled(1, 1, PixelFormat::Xrgb8888, |_, _| single_pixel_bgra([0, 0, 0, 255]));
        let view = view_of((1, 1), 1, None, Some((1280, 800))).expect("not the identity");
        let mut out = Shadow::empty();
        let written = out.scale_from(&one, &view, None);
        assert_eq!(written, vec![Rect::new(0, 0, 1280, 800)]);
        assert_eq!((out.width(), out.height()), (1280, 800));
        assert!(out.pixels().chunks_exact(4).all(|p| p == [0, 0, 0, 255]));
        assert_eq!(out.format(), PixelFormat::Xrgb8888);
    }

    /// The colour arrives in B, G, R, A order, and alpha is kept for the blend.
    #[test]
    fn a_translucent_single_pixel_keeps_its_alpha() {
        assert_eq!(single_pixel_bgra([0x80, 0x10, 0x20, 0x80]), [0x20, 0x10, 0x80, 0x80]);
        let half = filled(1, 1, PixelFormat::Argb8888, |_, _| [0, 0, 0x80, 0x80]);
        let view = view_of((1, 1), 1, None, Some((2, 1))).unwrap();
        let mut over = Shadow::empty();
        over.scale_from(&half, &view, None);
        let mut canvas = filled(2, 1, PixelFormat::Xrgb8888, |_, _| [0xFF, 0, 0, 0xFF]);
        canvas.compose_over(&over, (0, 0), EVERYWHERE);
        // Premultiplied half red over opaque blue: half of each (the blue's
        // 255 * 127/255 rounds to 0x7F).
        assert_eq!(&canvas.pixels()[0..4], &[0x7F, 0, 0x80, 0xFF]);
    }

    /// No viewport, or one that shows the buffer as it is, is the old path.
    #[test]
    fn the_identity_is_not_a_view() {
        assert_eq!(view_of((64, 32), 1, None, None), None);
        assert_eq!(view_of((64, 32), 2, None, None), None, "scale alone changes nothing");
        assert_eq!(view_of((64, 32), 1, Some((0.0, 0.0, 64.0, 32.0)), Some((64, 32))), None);
        assert_eq!(view_of((64, 32), 1, None, Some((64, 32))), None);
        assert_eq!(view_of((64, 32), 1, Some((0.0, 0.0, 64.0, 32.0)), None), None);
    }

    /// A source rectangle without a destination crops; the surface is the
    /// rectangle's size, and its pixels are the rectangle's.
    #[test]
    fn a_source_alone_crops() {
        let src = filled(4, 4, PixelFormat::Xrgb8888, |x, y| [x as u8, y as u8, 0, 255]);
        let view = view_of((4, 4), 1, Some((1.0, 2.0, 2.0, 2.0)), None).unwrap();
        assert_eq!(view.dst, (2, 2));
        let mut out = Shadow::empty();
        out.scale_from(&src, &view, None);
        assert_eq!(&out.pixels()[0..4], &[1, 2, 0, 255]);
        assert_eq!(&out.pixels()[12..16], &[2, 3, 0, 255]);
    }

    /// The source rectangle is in surface coordinates, so the buffer scale
    /// multiplies it into buffer pixels.
    #[test]
    fn the_source_is_scaled_into_buffer_pixels() {
        let view = view_of((8, 8), 2, Some((1.0, 1.0, 2.0, 2.0)), Some((2, 2))).unwrap();
        assert_eq!(view.src, (2.0, 2.0, 4.0, 4.0));
    }

    /// Doubling: every source pixel becomes a 2x2 block.
    #[test]
    fn a_destination_twice_the_source_doubles_each_pixel() {
        let src = filled(2, 1, PixelFormat::Xrgb8888, |x, _| [x as u8 * 100, 0, 0, 255]);
        let view = view_of((2, 1), 1, None, Some((4, 2))).unwrap();
        let mut out = Shadow::empty();
        out.scale_from(&src, &view, None);
        let firsts: Vec<u8> = out.pixels().chunks_exact(4).map(|p| p[0]).collect();
        assert_eq!(firsts, vec![0, 0, 100, 100, 0, 0, 100, 100]);
    }

    /// §104's follow-up: a stack recomposed in parts — overlapping parts
    /// included — is the stack recomposed whole, for a nested KWin's shape (a
    /// black root, a translucent layer over it, a small opaque one over that)
    /// and for a tree whose first surface does not cover the canvas.
    #[test]
    fn a_stack_recomposed_in_parts_is_the_stack_recomposed_whole() {
        let root = filled(40, 30, PixelFormat::Xrgb8888, |_, _| [0, 0, 0, 255]);
        let mut layer = filled(40, 30, PixelFormat::Argb8888, |x, y| {
            let a: u8 = if (x + y) % 3 == 0 { 255 } else { 0x80 };
            [(x * 3) as u8 % a, (y * 5) as u8 % a, 7 % a, a]
        });
        let cursor = filled(6, 6, PixelFormat::Argb8888, |_, _| [9, 9, 9, 255]);
        let overhang = filled(10, 10, PixelFormat::Argb8888, |_, _| [0, 0x40, 0, 0x40]);
        for stack in [
            vec![(Some(&root), (0, 0)), (Some(&layer), (0, 0)), (Some(&cursor), (17, 11))],
            // The first surface does not reach the canvas's right edge, and a
            // translucent child hangs over it: those pixels start clear.
            vec![(Some(&cursor), (0, 0)), (None, (0, 0)), (Some(&overhang), (34, 24))],
        ] {
            let stack: Vec<(Option<&Shadow>, (i32, i32))> = stack;
            let mut whole = Shadow::empty();
            whole.reset(40, 30, PixelFormat::Xrgb8888);
            whole.compose_stack(&stack, Rect::new(0, 0, 40, 30));
            let mut parts = Shadow::empty();
            parts.reset(40, 30, PixelFormat::Xrgb8888);
            // Garbage first: a part must not depend on what was there.
            parts.pixels.fill(0x5A);
            for r in [Rect::new(0, 0, 25, 30), Rect::new(10, 5, 30, 20), Rect::new(20, 0, 20, 30)] {
                parts.compose_stack(&stack, r);
            }
            assert_eq!(parts.pixels(), whole.pixels());
        }

        // And after a change in a damaged part, recomposing that part is
        // recomposing everything.
        let mut canvas = Shadow::empty();
        canvas.reset(40, 30, PixelFormat::Xrgb8888);
        fn stack<'a>(root: &'a Shadow, l: &'a Shadow) -> Vec<(Option<&'a Shadow>, (i32, i32))> {
            vec![(Some(root), (0, 0)), (Some(l), (0, 0))]
        }
        canvas.compose_stack(&stack(&root, &layer), Rect::new(0, 0, 40, 30));
        let damage = Rect::new(3, 4, 8, 6);
        let next = filled(8, 6, PixelFormat::Argb8888, |_, _| [0x20, 0, 0, 0x20]);
        let spec = BufferSpec { width: 40, height: 30, stride: 160, format: PixelFormat::Argb8888 };
        let mut src = layer.pixels().to_vec();
        for y in 0..6 {
            let d = ((4 + y) * 160 + 3 * 4) as usize;
            src[d..d + 32].copy_from_slice(&next.pixels()[(y * 32) as usize..(y * 32 + 32) as usize]);
        }
        layer.blit(&src, spec, &[damage]);
        canvas.compose_stack(&stack(&root, &layer), damage);
        let mut fresh = Shadow::empty();
        fresh.reset(40, 30, PixelFormat::Xrgb8888);
        fresh.compose_stack(&stack(&root, &layer), Rect::new(0, 0, 40, 30));
        assert_eq!(canvas.pixels(), fresh.pixels());
    }

    /// Damage in buffer pixels lands, widened, on the surface pixels it
    /// covers; and only those are redrawn.
    #[test]
    fn damage_is_mapped_and_limits_the_redraw() {
        let view = view_of((10, 10), 1, None, Some((20, 20))).unwrap();
        assert_eq!(view.map_damage(&[Rect::new(2, 2, 1, 1)]), vec![Rect::new(3, 3, 4, 4)]);
        assert_eq!(view.map_damage(&[Rect::new(50, 50, 1, 1)]), vec![]);

        let a = filled(10, 10, PixelFormat::Xrgb8888, |_, _| [1, 1, 1, 255]);
        let b = filled(10, 10, PixelFormat::Xrgb8888, |_, _| [9, 9, 9, 255]);
        let mut out = Shadow::empty();
        out.scale_from(&a, &view, None);
        let w = out.scale_from(&b, &view, Some(&[Rect::new(0, 0, 2, 2)]));
        assert_eq!(w, vec![Rect::new(0, 0, 2, 2)]);
        assert_eq!(&out.pixels()[0..4], &[9, 9, 9, 255]);
        assert_eq!(&out.pixels()[8..12], &[1, 1, 1, 255], "outside the damage");
    }
}

// ---------------------------------------------------------------------------
// STAGE 33: telling a client that decorates itself from one that does not
// ---------------------------------------------------------------------------

/// What the compositor can say about who is drawing this window's title bar.
///
/// # Why this is a guess with a name rather than a fact
///
/// §33 measured `weston-terminal` against this compositor with
/// `--decorations server`: it binds **neither** `org_kde_kwin_server_
/// decoration_manager` nor `zxdg_decoration_manager_v1`, sends zero decoration
/// requests, and draws its own title bar under Windows' — the double bar the
/// user reported. `weston-flower`, the *same toolkit* against the *same*
/// compositor in the same minute, comes out with one bar.
///
/// So the compositor cannot ask. The two protocols that carry the answer are
/// the two this class of client ignores, and there is no third. What is left is
/// one correlate, and it is the only thing on the wire that separated those two
/// traces:
///
/// | | `weston-terminal` | `weston-flower` |
/// |---|---|---|
/// | globals bound | the same seven | the same seven |
/// | decoration requests | 0 | 0 |
/// | `set_window_geometry` | **(32, 32, 742, 427)** | **(0, 0, 200, 200)** |
/// | title bars on screen | 2 | 1 |
///
/// A non-zero window-geometry **origin** means the buffer is bigger than the
/// window on the top and the left: the client is painting something outside its
/// own window, and in practice that something is a drop shadow, which is drawn
/// by clients that decorate themselves and by nothing else. §22 measured
/// Firefox under client-side decorations at (26, 23), which is the same signal
/// from a toolkit that shares no code with weston's.
///
/// **Where it fails, stated rather than discovered later.** A `weston-terminal`
/// that is *maximised* sets its margin to 0, so the inset goes to (0, 0) while
/// the title bar stays — the tell vanishes and the symptom does not. And a
/// client could draw a shadow and no title bar. So this is a diagnosis to
/// print, and an input to a per-application decision, not a switch to hang a
/// window style on by itself. §33.6 is the argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decorator {
    /// The client used a decoration protocol, so it was *told* and agreed —
    /// and what it agreed to is that **the system** draws the title bar.
    /// Whatever it draws, it is not drawing it in ignorance.
    Negotiated,
    /// The client used a decoration protocol and the mode it ended up with is
    /// **`Client`**: it asked to draw its own title bar and was answered yes.
    ///
    /// ADDED FOR §82. Before it, this was part of [`Self::Negotiated`] and was
    /// never folded under `auto`, which is why Firefox's tabs reached the
    /// caption only with `--decorations tabs`: GTK binds KDE's manager, so
    /// Firefox is always "negotiated", and the one bit that says whether it is
    /// about to draw a tab strip — the mode it asked for — was printed and then
    /// thrown away. It is the cleanest input this function has, because it is
    /// not a correlate: the client *said* it decorates itself, on the protocol
    /// that exists for saying it.
    AskedForClient,
    /// No decoration protocol and no margin around the window: nothing suggests
    /// this client is drawing a title bar. `weston-flower`.
    ProbablySystem,
    /// No decoration protocol, and a margin around the window. This client is
    /// almost certainly drawing its own title bar, has never been told the
    /// system would draw one, and the window has two. `weston-terminal`.
    ProbablyItself,
}

/// The decoration mode a surface and this compositor settled on, if they spoke
/// about it at all.
///
/// ADDED FOR §82. It is the *last* answer the compositor gave on either
/// protocol: KDE's object starts at the manager's default and moves with every
/// echoed `request_mode`, and `xdg-decoration` is whatever this compositor
/// configured. Kept per surface by `State` and read once, at map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agreed {
    /// No decoration object was ever created. `weston-terminal`.
    Nothing,
    /// The system draws the title bar. Firefox with
    /// `MOZ_GTK_TITLEBAR_DECORATION=none`.
    Server,
    /// The client draws its own. Firefox with nothing set, or with
    /// `MOZ_GTK_TITLEBAR_DECORATION=client`, which is what the launcher sets
    /// for tabs in the title bar.
    Client,
}

/// Which of the four this window is, from the two facts a compositor has at
/// map time.
///
/// `inset` is the window-geometry origin — `xdg_surface.set_window_geometry`'s
/// x and y — and `agreed` is the decoration mode this surface ended up with on
/// either protocol, or [`Agreed::Nothing`] if it never created a decoration
/// object.
pub fn decorator(inset: (i32, i32), agreed: Agreed) -> Decorator {
    match agreed {
        Agreed::Server => return Decorator::Negotiated,
        Agreed::Client => return Decorator::AskedForClient,
        Agreed::Nothing => {}
    }
    // Either axis. A client with a shadow on the top and none on the left is
    // still a client with a shadow, and `> 0` on both would miss it.
    if inset.0 > 0 || inset.1 > 0 {
        Decorator::ProbablyItself
    } else {
        Decorator::ProbablySystem
    }
}

/// How many title bars this window is going to have.
///
/// ADDED WHEN §31 MERGED, and it is the correction that merge forced. §33 was
/// written against a tree where the caption was never folded, so
/// [`Decorator::ProbablyItself`] and "two title bars" were the same statement
/// and the report said so in as many words. §31's `--decorations tabs` makes
/// `WM_NCCALCSIZE` fold the caption into the client area, and then a client that
/// decorates itself is **exactly right** — its bar is the window's — while a
/// client that decorates itself under `server` is the user's bug.
///
/// So the verdict is the pair, not either half. The case that reverses is the
/// one worth having a name for: under `tabs`, a client with no bar of its own
/// gets a window with **no title bar at all**, because the caption it would
/// have been given has been folded away and there is nothing in the strip. §31's
/// flag is per-run and a run has both kinds of client in it, so that is not a
/// hypothetical; it is what `weston-flower` would get from a run configured for
/// Firefox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bars {
    /// One, and it is Windows'.
    OneSystem,
    /// One, and it is the client's, sitting in the folded caption. §31's
    /// arrangement, working.
    OneClient,
    /// Two, one above the other. The bug the user reported.
    Two,
    /// **None.** The caption was folded away and the client draws nothing into
    /// the strip it left.
    None,
    /// The client negotiated, so it agreed to whatever it is doing and this is
    /// not a guess to be reported as a verdict.
    Agreed,
}

/// The verdict, from who decorates and whether this run folds the caption.
pub fn bars(who: Decorator, extends_frame: bool) -> Bars {
    match (who, extends_frame) {
        (Decorator::Negotiated, _) => Bars::Agreed,
        // §82. A client that asked for `Client` and was told yes is drawing a
        // bar, so it is judged like the client that draws one unasked: right
        // when the caption is folded, two bars when it is not. The second is
        // `--decorations server` or `client` with a client that insisted —
        // §22.4's documented cost, and now it says so instead of "agreed".
        (Decorator::ProbablyItself | Decorator::AskedForClient, false) => Bars::Two,
        (Decorator::ProbablyItself | Decorator::AskedForClient, true) => Bars::OneClient,
        (Decorator::ProbablySystem, false) => Bars::OneSystem,
        (Decorator::ProbablySystem, true) => Bars::None,
    }
}

/// **Should *this* window's caption be folded into its client area?**
///
/// ADDED FOR §46, and it is the join §33.7 said was missing and could not make:
/// §31's arrangement is right for a client that decorates itself and wrong for
/// one that does not, and `--decorations` is a **per-run** flag while which kind
/// of client this is is a **per-window** fact. This function is the whole of the
/// join, and every arm of it is a decision somebody has to be able to argue
/// with:
///
/// * `Tabs` folds unconditionally, which is §31's behaviour unchanged. It is
///   kept because it is the control leg every claim about the other arms is
///   measured against, and because a run that wants to photograph one specific
///   client's frame should not have to satisfy a heuristic to do it.
/// * `Server` and `Client` never fold, which is stage 17's arrangement and,
///   for `Client`, the deliberate two-bar control leg.
/// * `Auto` folds for exactly [`Decorator::ProbablyItself`] — a client that
///   bound no decoration protocol *and* reports a margin around its window.
///
/// # Why `Auto` is safe for the clients that are already right
///
/// A client that negotiated is never folded, whatever it drew. Firefox binds
/// KDE's manager, reads `default_mode`, and is therefore `Negotiated` on every
/// run — so `Auto` is byte-for-byte `Server` for it, which is the arrangement
/// §22 measured and shipped. A client that draws no bar and has no margin is
/// `ProbablySystem` and keeps Windows' caption. Only the third kind moves, and
/// the third kind is the one with two title bars today.
///
/// # Why the height of the client's bar does not enter into it
///
/// It cannot make the count wrong. Folding puts the client's surface at the top
/// of the window, so whatever the client draws there *is* the window's title
/// bar, at whatever height the client chose. §31.9's 46-into-31 mismatch decides
/// where Windows' buttons sit relative to the client's widgets; it does not
/// decide how many bars there are.
///
/// # Why the decision is taken once, at map, and never revisited
///
/// §33.6 refused to hang a window style on the inset because *"a maximised
/// `weston-terminal` sets its margin to zero: the tell vanishes and the symptom
/// does not"* — a style that flipped on the live inset would unfold the caption
/// when the user maximised the window and fold it back on restore. Deciding
/// once, from the inset at first map, is what makes that objection not apply:
/// the window is not maximised at map, the answer is taken then, and the frame
/// does not change under the user afterwards. The cost is a client that starts
/// maximised and decorates itself, which is diagnosed rather than handled — the
/// map-time report says which way it went and §46.6 says what it costs.
///
/// # §82: and a client that *asked* to decorate itself
///
/// The paragraph above said Firefox is `Negotiated` on every run, and that was
/// true only because the mode it negotiated was thrown away. Firefox started
/// without `MOZ_GTK_TITLEBAR_DECORATION=none` asks for `Client` whatever the
/// manager's default (§22.3's table), the compositor echoes it (§22.4), and
/// the window then had two title bars under `auto` — Windows' caption, and the
/// tab strip under it. That client is now [`Decorator::AskedForClient`] and is
/// folded, which is what puts Firefox's tabs in the caption under the default
/// rather than under a flag. The client that asked for `Server` is unchanged,
/// so `MOZ_GTK_TITLEBAR_DECORATION=none` is still the per-application opt-out
/// it always was, and a run can contain both.
pub fn fold_caption(mode_folds: bool, mode_is_auto: bool, who: Decorator) -> bool {
    if mode_folds {
        return true;
    }
    mode_is_auto && matches!(who, Decorator::ProbablyItself | Decorator::AskedForClient)
}

#[cfg(test)]
mod decorator_tests {
    use super::*;

    /// The two clients §33 measured, by their measured numbers.
    #[test]
    fn the_two_weston_clients_come_out_different() {
        // weston-terminal: set_window_geometry(32, 32, 742, 427), no protocol.
        assert_eq!(decorator((32, 32), Agreed::Nothing), Decorator::ProbablyItself);
        // weston-flower: set_window_geometry(0, 0, 200, 200), no protocol.
        assert_eq!(decorator((0, 0), Agreed::Nothing), Decorator::ProbablySystem);
    }

    /// §22's Firefox, from a toolkit that shares no code with weston's. The
    /// inset is not square, which is why this is two numbers and not one.
    #[test]
    fn firefox_under_client_side_decorations() {
        assert_eq!(decorator((26, 23), Agreed::Nothing), Decorator::ProbablyItself);
    }

    /// One axis is enough.
    #[test]
    fn a_margin_on_one_side_only_still_counts() {
        assert_eq!(decorator((0, 23), Agreed::Nothing), Decorator::ProbablyItself);
        assert_eq!(decorator((26, 0), Agreed::Nothing), Decorator::ProbablyItself);
    }

    /// A client that spoke a decoration protocol is never guessed about, even
    /// when it has a shadow — Firefox under `--decorations client` is exactly
    /// that, and it is drawing its own bar *because it was told to*.
    #[test]
    fn negotiation_beats_the_correlate() {
        assert_eq!(decorator((26, 23), Agreed::Server), Decorator::Negotiated);
        assert_eq!(decorator((0, 0), Agreed::Server), Decorator::Negotiated);
    }

    /// A negative inset is not a shadow. `set_window_geometry` is clamped to
    /// the surface by smithay long before this, so this is a guard against the
    /// arithmetic rather than against a client — but `!= 0` would call it a
    /// shadow and that would be wrong.
    #[test]
    fn a_negative_origin_is_not_a_margin() {
        assert_eq!(decorator((-4, -4), Agreed::Nothing), Decorator::ProbablySystem);
    }

    /// The two §33 clients under both of §31's arrangements, which is the whole
    /// table and the reason `bars` is a pair rather than a property of the
    /// client.
    #[test]
    fn the_verdict_reverses_when_the_caption_is_folded() {
        let terminal = decorator((32, 32), Agreed::Nothing);
        let flower = decorator((0, 0), Agreed::Nothing);

        // `--decorations server`: what the user photographed.
        assert_eq!(bars(terminal, false), Bars::Two);
        assert_eq!(bars(flower, false), Bars::OneSystem);

        // `--decorations tabs`: the terminal becomes right and the flower
        // becomes wrong. Same two clients, same compositor, one flag.
        assert_eq!(bars(terminal, true), Bars::OneClient);
        assert_eq!(bars(flower, true), Bars::None);
    }

    /// A client that negotiated is never given a verdict, under either
    /// arrangement: it agreed to what it is doing.
    #[test]
    fn a_negotiated_client_is_not_judged() {
        let fx = decorator((26, 23), Agreed::Server);
        assert_eq!(bars(fx, false), Bars::Agreed);
        assert_eq!(bars(fx, true), Bars::Agreed);
    }

    // --- §46: the per-window join ------------------------------------------

    /// `auto` folds for the client with two title bars and for nothing else.
    /// This is the whole of the bug fix, as a table.
    #[test]
    fn auto_folds_only_for_the_client_that_decorates_itself() {
        let terminal = decorator((32, 32), Agreed::Nothing); // ProbablyItself
        let flower = decorator((0, 0), Agreed::Nothing); //     ProbablySystem
        let firefox = decorator((26, 23), Agreed::Server); //   Negotiated

        assert!(fold_caption(false, true, terminal));
        assert!(!fold_caption(false, true, flower));
        assert!(!fold_caption(false, true, firefox));
    }

    /// …and every window comes out with exactly one title bar, which is the
    /// claim the fix is actually making. Asserted through `bars` rather than
    /// restated, so that a change to either function has to keep them agreeing.
    #[test]
    fn under_auto_all_three_clients_get_one_bar() {
        for (who, want) in [
            (decorator((32, 32), Agreed::Nothing), Bars::OneClient),
            (decorator((0, 0), Agreed::Nothing), Bars::OneSystem),
            (decorator((26, 23), Agreed::Server), Bars::Agreed),
        ] {
            assert_eq!(bars(who, fold_caption(false, true, who)), want);
        }
    }

    /// `tabs` still folds for everything, because it is the control leg — and
    /// that is exactly why it is not the default: it is the run that gives
    /// `weston-flower` no title bar at all.
    #[test]
    fn tabs_is_unconditional_and_that_is_why_it_is_not_the_default() {
        let flower = decorator((0, 0), Agreed::Nothing);
        assert!(fold_caption(true, false, flower));
        assert_eq!(bars(flower, fold_caption(true, false, flower)), Bars::None);
    }

    /// `server` and `client` fold nothing, whoever the client is. `client` is
    /// the deliberate two-bar leg and must stay one.
    #[test]
    fn the_named_arrangements_are_unchanged() {
        let terminal = decorator((32, 32), Agreed::Nothing);
        assert!(!fold_caption(false, false, terminal));
        assert_eq!(bars(terminal, fold_caption(false, false, terminal)), Bars::Two);
    }

    // --- §82: Firefox's tabs in the caption under the default ----------------

    /// The mode a client agreed to beats any inset, in both directions. The
    /// inset for Firefox at map is whatever the geometry cache held, and
    /// `report_decorator` logged `0,0` for a Firefox whose trace said
    /// `(26, 23)` — so the decision must not lean on it when the client said.
    #[test]
    fn the_agreed_mode_decides_and_the_inset_does_not() {
        for inset in [(0, 0), (26, 23)] {
            assert_eq!(decorator(inset, Agreed::Client), Decorator::AskedForClient);
            assert_eq!(decorator(inset, Agreed::Server), Decorator::Negotiated);
        }
    }

    /// The default arrangement, for the two Firefoxes the launcher can start:
    /// `titlebar = tabs` (it asks for `Client`) folds, `titlebar = none` (it
    /// asks for `Server`) does not. One run, one flag, both right.
    #[test]
    fn auto_folds_firefox_that_asked_for_client_and_not_the_one_that_opted_out() {
        let tabs = decorator((26, 23), Agreed::Client);
        let opted_out = decorator((0, 0), Agreed::Server);
        assert!(fold_caption(false, true, tabs));
        assert!(!fold_caption(false, true, opted_out));
        assert_eq!(bars(tabs, fold_caption(false, true, tabs)), Bars::OneClient);
        assert_eq!(bars(opted_out, fold_caption(false, true, opted_out)), Bars::Agreed);
    }

    /// Under the named legs the insisting client keeps §22.4's cost — and the
    /// verdict now says "two" rather than "agreed", because it is two.
    #[test]
    fn server_and_client_legs_do_not_fold_an_insisting_client() {
        let fx = decorator((26, 23), Agreed::Client);
        assert!(!fold_caption(false, false, fx));
        assert_eq!(bars(fx, false), Bars::Two);
        // `tabs` folds it, as it always did.
        assert!(fold_caption(true, false, fx));
        assert_eq!(bars(fx, true), Bars::OneClient);
    }

    /// Nothing under `auto` has two bars or none any more, across all four
    /// kinds of client — the claim §46 made for three, extended to the fourth.
    #[test]
    fn under_auto_every_kind_of_client_gets_one_bar() {
        for agreed in [Agreed::Nothing, Agreed::Server, Agreed::Client] {
            for inset in [(0, 0), (32, 32)] {
                let who = decorator(inset, agreed);
                let verdict = bars(who, fold_caption(false, true, who));
                assert!(
                    !matches!(verdict, Bars::Two | Bars::None),
                    "{agreed:?} {inset:?} -> {verdict:?}"
                );
            }
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

    fn shadow_of(w: i32, h: i32, format: PixelFormat, px: [u8; 4]) -> Shadow {
        let mut s = Shadow::empty();
        let spec = BufferSpec { width: w, height: h, stride: w * 4, format };
        s.blit(&px.repeat((w * h) as usize), spec, &[]);
        s
    }

    #[test]
    fn a_translucent_subsurface_is_drawn_over_its_parent_not_instead_of_it() {
        // §95: Firefox's <select> dropdown, a subsurface with a transparent
        // shadow margin. Copied, the margin was a black frame over the page.
        let mut canvas = shadow_of(4, 1, PixelFormat::Argb8888, [200, 100, 50, 255]);
        let mut child = Shadow::empty();
        let spec = BufferSpec { width: 3, height: 1, stride: 12, format: PixelFormat::Argb8888 };
        // Transparent, half-covered (premultiplied 50% white), opaque blue.
        child.blit(&[0, 0, 0, 0, 128, 128, 128, 128, 255, 0, 0, 255], spec, &[]);
        canvas.compose_over(&child, (1, 0), EVERYWHERE);
        assert_eq!(&canvas.pixels()[0..4], &[200, 100, 50, 255], "outside the child");
        assert_eq!(&canvas.pixels()[4..8], &[200, 100, 50, 255], "under a transparent pixel");
        // 128 + 200 * 127/255, and so on: the page shows through at half.
        assert_eq!(&canvas.pixels()[8..12], &[228, 178, 153, 255]);
        assert_eq!(&canvas.pixels()[12..16], &[255, 0, 0, 255], "an opaque pixel replaces");
    }

    #[test]
    fn an_xrgb_subsurface_is_copied_whatever_its_padding_byte_says() {
        let mut canvas = shadow_of(2, 1, PixelFormat::Argb8888, [9, 9, 9, 255]);
        // Padding of zero must not read as "transparent".
        let child = shadow_of(2, 1, PixelFormat::Xrgb8888, [1, 2, 3, 0]);
        canvas.compose_over(&child, (0, 0), EVERYWHERE);
        assert_eq!(canvas.pixels(), &[1, 2, 3, 0, 1, 2, 3, 0]);
    }

    #[test]
    fn an_opaque_subsurface_draws_exactly_as_the_copy_did() {
        let mut a = shadow_of(3, 2, PixelFormat::Argb8888, [5, 6, 7, 255]);
        let mut b = shadow_of(3, 2, PixelFormat::Argb8888, [5, 6, 7, 255]);
        let child = shadow_of(2, 2, PixelFormat::Argb8888, [40, 50, 60, 255]);
        a.compose(&child, (1, 1), EVERYWHERE);
        b.compose_over(&child, (1, 1), EVERYWHERE);
        assert_eq!(a.pixels(), b.pixels());
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
