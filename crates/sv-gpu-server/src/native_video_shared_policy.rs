//! Portable nonce, allocation-bound, and original-layout checks for exports.
//! Native IOSurface kind/geometry validation precedes these identity checks.
use crate::video_nv12::Layout;

/// Monotonic owner-generation validity. Retained allocations survive revocation,
/// but an import beginning after observed revocation must fail.
#[derive(Default)]
pub(crate) struct Revocation(std::sync::atomic::AtomicBool);
impl Revocation {
    pub(crate) fn revoke(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn active(&self) -> bool {
        !self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

pub(crate) fn authorize(
    expected: &[u8; 16],
    allocation: u64,
    nonce: &[u8; 16],
    size: u64,
) -> Result<(), String> {
    // Inspect every nonce byte before deciding; never print capability secrets.
    let differs = expected
        .iter()
        .zip(nonce)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b));
    if differs != 0 {
        return Err("NV12 export nonce mismatch".into());
    }
    if size > allocation {
        return Err("NV12 export requested size exceeds original allocation".into());
    }
    Ok(())
}

pub(crate) fn validate_layout(layout: Layout, original: Layout) -> Result<u64, String> {
    if original.width != layout.width
        || original.height != layout.height
        || original.allocation_size != layout.allocation_size
    {
        return Err("NV12 export surface differs from original picture allocation".into());
    }
    if original.planes != layout.planes {
        return Err("NV12 export plane differs from original picture layout".into());
    }
    u64::try_from(original.allocation_size)
        .map_err(|_| "NV12 export allocation size overflow".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revocation_blocks_new_imports_without_dropping_retained_owners() {
        let owner = std::sync::Arc::new(Revocation::default());
        let accepted = std::sync::Arc::clone(&owner);
        assert!(owner.active());
        owner.revoke();
        assert!(!owner.active());
        assert!(!accepted.active());
        assert_eq!(std::sync::Arc::strong_count(&owner), 2);
        owner.revoke();
        assert!(!owner.active());
    }

    #[test]
    fn nonce_and_original_allocation_bound_are_both_required() {
        let nonce = [0x97; 16];
        assert!(authorize(&nonce, 8192, &nonce, 8192).is_ok());
        assert!(authorize(&nonce, 8192, &nonce, 4096).is_ok());
        assert!(authorize(&nonce, 8192, &nonce, 8193).is_err());
        for index in 0..16 {
            let mut wrong = nonce;
            wrong[index] ^= 1;
            assert!(authorize(&nonce, 8192, &wrong, 4096).is_err());
        }
    }
    #[test]
    fn exported_metadata_must_match_original_offsets_pitches_and_size() {
        use crate::video_nv12::Plane;
        let layout = Layout {
            width: 64,
            height: 64,
            allocation_size: 8192,
            planes: [
                Plane {
                    width: 64,
                    height: 64,
                    offset: 0,
                    stride: 64,
                },
                Plane {
                    width: 32,
                    height: 32,
                    offset: 4096,
                    stride: 64,
                },
            ],
        };
        assert_eq!(validate_layout(layout, layout).unwrap(), 8192);
        let mut wrong = layout;
        wrong.planes[1].offset += 64;
        assert!(validate_layout(layout, wrong).is_err());
        let mut wrong = layout;
        wrong.planes[1].stride += 64;
        assert!(validate_layout(layout, wrong).is_err());
        let mut wrong = layout;
        wrong.allocation_size += 4096;
        assert!(validate_layout(layout, wrong).is_err());
        let mut wrong = layout;
        wrong.planes[0].width -= 1;
        assert!(validate_layout(layout, wrong).is_err());
    }
}
