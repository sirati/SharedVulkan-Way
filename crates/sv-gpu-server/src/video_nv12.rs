//! Validated NV12 plane geometry shared by the VideoToolbox import boundary.
//! Offsets and strides are supplied by the owner, never inferred from width.
//! This does not advertise Vulkan Video or a multi-plane Vulkan image.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plane {
    pub width: usize,
    pub height: usize,
    pub offset: usize,
    pub stride: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub width: usize,
    pub height: usize,
    pub allocation_size: usize,
    pub planes: [Plane; 2],
}
impl Layout {
    pub fn validate(self) -> Result<Self, &'static str> {
        if self.width == 0 || self.height == 0 { return Err("empty NV12 picture"); }
        let expected = [(self.width, self.height, 1usize),
            (self.width.div_ceil(2), self.height.div_ceil(2), 2usize)];
        let mut ends = [0; 2];
        for (i, (p, &(w, h, bpp))) in self.planes.iter().zip(&expected).enumerate() {
            if (p.width, p.height) != (w, h) { return Err("NV12 plane dimensions"); }
            let row = w.checked_mul(bpp).ok_or("NV12 row overflow")?;
            if p.stride < row { return Err("NV12 short stride"); }
            ends[i] = p.stride.checked_mul(h - 1).and_then(|n| n.checked_add(row))
                .and_then(|n| p.offset.checked_add(n)).ok_or("NV12 plane overflow")?;
            if ends[i] > self.allocation_size { return Err("NV12 plane outside allocation"); }
        }
        if self.planes[0].offset < ends[1] && self.planes[1].offset < ends[0] {
            return Err("NV12 planes overlap");
        }
        Ok(self)
    }
}

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub mod videotoolbox;
pub mod h264_parameters;

#[cfg(test)]
mod tests {
    use super::*;
    fn padded() -> Layout {
        Layout { width: 5, height: 3, allocation_size: 512, planes: [
            Plane { width: 5, height: 3, offset: 16, stride: 64 },
            Plane { width: 3, height: 2, offset: 256, stride: 128 }] }
    }
    #[test] fn odd_dimensions_and_independent_strides_are_preserved() {
        let l = padded(); assert_eq!(l.validate(), Ok(l));
    }
    #[test] fn rejects_truncated_chroma() {
        let mut l = padded(); l.allocation_size = 389;
        assert_eq!(l.validate(), Err("NV12 plane outside allocation"));
    }
    #[test] fn rejects_overlap_and_short_stride() {
        let mut l = padded(); l.planes[1].offset = 128;
        assert_eq!(l.validate(), Err("NV12 planes overlap"));
        l = padded(); l.planes[1].stride = 5;
        assert_eq!(l.validate(), Err("NV12 short stride"));
    }
    #[test] fn rejects_overflow_without_wraparound() {
        let mut l = padded(); l.planes[1].offset = usize::MAX - 1;
        assert_eq!(l.validate(), Err("NV12 plane overflow"));
    }
    #[test] fn rejects_wrong_chroma_geometry() {
        let mut l = padded(); l.planes[1].width = 5;
        assert_eq!(l.validate(), Err("NV12 plane dimensions"));
    }
}

#[cfg(target_os = "macos")]
pub mod joined;
