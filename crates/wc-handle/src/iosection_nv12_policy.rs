//! Metadata-only contract for the original joined 64x64 video target.
//! Validation grants no broker permission and never reads or copies pixels.
use std::ops::Range;

pub const FOURCC_420V: u32 = u32::from_be_bytes(*b"420v");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plane {
    pub width: usize,
    pub height: usize,
    pub bytes_per_element: usize,
    pub bytes_per_row: usize,
    pub offset: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Surface {
    pub width: usize,
    pub height: usize,
    pub pixel_format: u32,
    pub allocation_size: usize,
    pub plane_count: usize,
    pub planes: [Plane; 2],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    SurfaceShape,
    PlaneCount,
    PlaneShape,
    RowPitch,
    AllocationBounds,
    OverlappingPlanes,
}

/// Validated original offsets and pitches, with no derived replacement layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nv12Geometry(Surface);
impl Nv12Geometry {
    pub fn validate(surface: Surface) -> Result<Self, Rejected> {
        if surface.width != 64 || surface.height != 64 || surface.pixel_format != FOURCC_420V {
            return Err(Rejected::SurfaceShape);
        }
        if surface.allocation_size == 0 || surface.allocation_size > isize::MAX as usize {
            return Err(Rejected::AllocationBounds);
        }
        if surface.plane_count != 2 {
            return Err(Rejected::PlaneCount);
        }
        let [y, uv] = surface.planes;
        if (y.width, y.height, y.bytes_per_element) != (64, 64, 1)
            || (uv.width, uv.height, uv.bytes_per_element) != (32, 32, 2)
        {
            return Err(Rejected::PlaneShape);
        }
        let y_span = plane_span(y, surface.allocation_size)?;
        let uv_span = plane_span(uv, surface.allocation_size)?;
        if y_span.start < uv_span.end && uv_span.start < y_span.end {
            return Err(Rejected::OverlappingPlanes);
        }
        Ok(Self(surface))
    }
    pub fn surface(self) -> Surface {
        self.0
    }
    pub fn allocation_size(self) -> usize {
        self.0.allocation_size
    }
    pub fn planes(self) -> [Plane; 2] {
        self.0.planes
    }
    /// Exact active bytes for one row, excluding its original pitch padding.
    pub fn row(self, plane: usize, row: usize) -> Option<Range<usize>> {
        let plane = self.0.planes.get(plane)?;
        if row >= plane.height {
            return None;
        }
        let start = plane
            .offset
            .checked_add(plane.bytes_per_row.checked_mul(row)?)?;
        let end = start.checked_add(plane.width.checked_mul(plane.bytes_per_element)?)?;
        (end <= self.0.allocation_size).then_some(start..end)
    }
}
fn plane_span(plane: Plane, allocation: usize) -> Result<Range<usize>, Rejected> {
    let row_bytes = plane
        .width
        .checked_mul(plane.bytes_per_element)
        .ok_or(Rejected::RowPitch)?;
    if plane.bytes_per_row < row_bytes {
        return Err(Rejected::RowPitch);
    }
    // Reserve every full original row, including padding. This rejects planes
    // sharing another plane's padding rather than assuming consumers ignore it.
    let size = plane
        .bytes_per_row
        .checked_mul(plane.height)
        .ok_or(Rejected::AllocationBounds)?;
    let end = plane
        .offset
        .checked_add(size)
        .ok_or(Rejected::AllocationBounds)?;
    if end > allocation {
        return Err(Rejected::AllocationBounds);
    }
    Ok(plane.offset..end)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn surface() -> Surface {
        Surface {
            width: 64,
            height: 64,
            pixel_format: FOURCC_420V,
            allocation_size: 8192,
            plane_count: 2,
            planes: [
                Plane {
                    width: 64,
                    height: 64,
                    bytes_per_element: 1,
                    bytes_per_row: 64,
                    offset: 0,
                },
                Plane {
                    width: 32,
                    height: 32,
                    bytes_per_element: 2,
                    bytes_per_row: 64,
                    offset: 4096,
                },
            ],
        }
    }
    #[test]
    fn original_plane_metadata_and_active_row_intervals_are_preserved() {
        let original = surface();
        let geometry = Nv12Geometry::validate(original).unwrap();
        assert_eq!(geometry.surface(), original);
        assert_eq!(geometry.row(0, 0), Some(0..64));
        assert_eq!(geometry.row(0, 63), Some(4032..4096));
        assert_eq!(geometry.row(1, 0), Some(4096..4160));
        assert_eq!(geometry.row(1, 31), Some(6080..6144));
        assert_eq!(geometry.row(0, 64), None);
        assert_eq!(geometry.row(2, 0), None);
    }
    #[test]
    fn padded_and_nonzero_original_offsets_are_not_repacked() {
        let mut original = surface();
        original.planes[0].offset = 128;
        original.planes[0].bytes_per_row = 128;
        original.planes[1].offset = 128 + 128 * 64;
        original.planes[1].bytes_per_row = 256;
        original.allocation_size = original.planes[1].offset + 256 * 32;
        let geometry = Nv12Geometry::validate(original).unwrap();
        assert_eq!(geometry.planes(), original.planes);
        assert_eq!(geometry.allocation_size(), original.allocation_size);
        assert_eq!(
            geometry.row(1, 31),
            Some(original.planes[1].offset + 256 * 31..original.planes[1].offset + 256 * 31 + 64)
        );
    }
    #[test]
    fn wrong_surface_and_plane_shapes_are_refused() {
        let mut wrong = surface();
        wrong.pixel_format = u32::from_be_bytes(*b"420f");
        assert_eq!(Nv12Geometry::validate(wrong), Err(Rejected::SurfaceShape));
        let mut wrong = surface();
        wrong.width = 128;
        assert_eq!(Nv12Geometry::validate(wrong), Err(Rejected::SurfaceShape));
        let mut wrong = surface();
        wrong.height = 32;
        assert_eq!(Nv12Geometry::validate(wrong), Err(Rejected::SurfaceShape));
        for count in [0, 1, 3, usize::MAX] {
            let mut wrong = surface();
            wrong.plane_count = count;
            assert_eq!(Nv12Geometry::validate(wrong), Err(Rejected::PlaneCount));
        }
        for index in 0..2 {
            let mut wrong = surface();
            wrong.planes[index].bytes_per_element += 1;
            assert_eq!(Nv12Geometry::validate(wrong), Err(Rejected::PlaneShape));
            let mut wrong = surface();
            wrong.planes[index].height -= 1;
            assert_eq!(Nv12Geometry::validate(wrong), Err(Rejected::PlaneShape));
            let mut wrong = surface();
            wrong.planes[index].bytes_per_row = 63;
            assert_eq!(Nv12Geometry::validate(wrong), Err(Rejected::RowPitch));
        }
    }
    #[test]
    fn overlap_including_padding_and_truncated_allocations_are_refused() {
        let mut wrong = surface();
        wrong.planes[1].offset = 4032;
        assert_eq!(
            Nv12Geometry::validate(wrong),
            Err(Rejected::OverlappingPlanes)
        );
        let mut wrong = surface();
        wrong.planes[0].bytes_per_row = 128;
        wrong.allocation_size = 16384;
        assert_eq!(
            Nv12Geometry::validate(wrong),
            Err(Rejected::OverlappingPlanes)
        );
        let mut wrong = surface();
        wrong.allocation_size = 6143;
        assert_eq!(
            Nv12Geometry::validate(wrong),
            Err(Rejected::AllocationBounds)
        );
    }
    #[test]
    fn integer_overflow_and_unaddressable_allocations_are_refused() {
        let mut wrong = surface();
        wrong.planes[1].offset = usize::MAX - 1;
        assert_eq!(
            Nv12Geometry::validate(wrong),
            Err(Rejected::AllocationBounds)
        );
        let mut wrong = surface();
        wrong.planes[0].bytes_per_row = usize::MAX;
        assert_eq!(
            Nv12Geometry::validate(wrong),
            Err(Rejected::AllocationBounds)
        );
        let mut wrong = surface();
        wrong.allocation_size = usize::MAX;
        assert_eq!(
            Nv12Geometry::validate(wrong),
            Err(Rejected::AllocationBounds)
        );
    }
}
