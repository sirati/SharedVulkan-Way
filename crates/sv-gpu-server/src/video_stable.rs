//! The C structures of a video decode, at addresses that stay valid for the
//! session: the fix for Intel's decode fault (docs/video-decode.md §7.11.5).
//!
//! # What it fixes
//!
//! Intel's Windows driver (`igvk64.dll` 101.7076) keeps a pointer into a
//! structure it was handed by an earlier `vkCmdBeginVideoCodingKHR` or
//! `vkCmdDecodeVideoKHR`, and follows it at a later IDR that retires the DPB.
//! Three guest runs faulted at the clip's packet-250 IDR. The memory behind the
//! pointer had been freed and reused, the last time by this side's own
//! allocations (our guard bytes in `r8`). One run with every video command's
//! raise pool kept alive (`SV_VIDEO_RETAIN=8192`) decoded 3029 pictures across
//! ten such IDRs without a fault.
//!
//! Native ffmpeg never meets this. It builds these structures inside its
//! per-picture private data (`FFVulkanDecodePicture`: the picture resource,
//! the setup slot, the codec's DPB slot info, the decode info and its
//! reference array). That memory comes from a buffer pool and is recycled,
//! never freed, so a stale pointer always reads *some* picture's structures of
//! the same type. The forwarder raised fresh copies for each call and freed
//! them when the call returned.
//!
//! # What it does
//!
//! Per video session there is one [`SlotStore`] per DPB slot index (and one for
//! index -1, a begin's not-yet-active picture). It holds that slot's
//! `VkVideoReferenceSlotInfoKHR`, `VkVideoPictureResourceInfoKHR`, H.264 DPB slot
//! info and std reference info. There is one [`PictureStore`] per setup slot
//! (and one for a decode with none). It holds the `VkVideoDecodeInfoKHR`, its
//! H.264 picture info, std picture info, slice offsets and reference array.
//! Each is boxed and never moves or is freed while the connection lives. Every
//! call copies what it was given into the store for its slot and hands the
//! driver pointers into the stores. A pointer the driver kept therefore always
//! reads the structures of the picture now in that slot, as natively.
//!
//! The begin's reference *array* is per call, as natively: ffmpeg builds it on
//! the stack. Its elements point into the stores.
//!
//! H.264 only. A chain this does not know (another codec's DPB slot info, an
//! inline-parameters link) leaves the call exactly as raised, and
//! [`crate::driver`] does not serve the other codecs on Intel's Windows driver.

use std::collections::HashMap;
use sv_gpu_entry::*;

/// `VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_DPB_SLOT_INFO_KHR`.
const H264_DPB_SLOT: i32 = 1_000_040_006;
/// `VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_PICTURE_INFO_KHR`.
const H264_PICTURE: i32 = 1_000_040_001;

/// The most reference slots one call may name, with room to spare: H.264 has
/// 16 DPB slots plus the setup one.
const MAX_REFS: usize = 33;

/// The structures of one DPB slot.
#[repr(C)]
pub struct SlotStore {
    slot: VkVideoReferenceSlotInfoKHR,
    picture: VkVideoPictureResourceInfoKHR,
    h264: VkVideoDecodeH264DpbSlotInfoKHR,
    h264_std: StdVideoDecodeH264ReferenceInfo,
}

/// The structures of one decode, by its setup slot.
#[repr(C)]
pub struct PictureStore {
    info: VkVideoDecodeInfoKHR,
    h264: VkVideoDecodeH264PictureInfoKHR,
    h264_std: StdVideoDecodeH264PictureInfo,
    refs: [VkVideoReferenceSlotInfoKHR; MAX_REFS],
    /// Slice offsets: `slices_cap` `u32`s, never reallocated in place. A
    /// larger count gets a new buffer, and the old one is kept in `retired`,
    /// because the driver may hold its address.
    slices: *mut u32,
    slices_cap: usize,
}

/// Every video session's stores, for one connection.
#[derive(Default)]
pub struct Stores {
    sessions: HashMap<u64, Session>,
    /// The session each command buffer's open coding scope belongs to, from
    /// its `vkCmdBeginVideoCodingKHR`.
    scope: HashMap<u64, u64>,
}

/// One session's stores. Each is a `Box::into_raw` allocation that is only
/// ever written through that raw pointer, never through a `&mut`, so the
/// pointers the driver keeps stay valid in Rust's aliasing model as well as in
/// fact; all are freed when the connection's `Stores` drops.
#[derive(Default)]
struct Session {
    slots: HashMap<i32, *mut SlotStore>,
    pictures: HashMap<i32, *mut PictureStore>,
    retired: Vec<(*mut u32, usize)>,
}

// SAFETY: the raw pointers point only into the stores' own allocations (or
// are copied driver handles); all access goes through the `Mutex` in
// `server::Shared`.
unsafe impl Send for Stores {}

fn zeroed_box<T>() -> *mut T {
    // SAFETY: only used for the plain C structures above, for which all-zero
    // is a valid (empty) value.
    Box::into_raw(Box::new(unsafe { std::mem::zeroed::<T>() }))
}

fn slice_buffer(cap: usize) -> *mut u32 {
    Box::into_raw(vec![0u32; cap].into_boxed_slice()) as *mut u32
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: every pointer came from `Box::into_raw` here and is freed
        // once; the device that might still read them is gone by now (the
        // connection's `Shared` drops after its sessions are torn down).
        unsafe {
            for (_, s) in self.slots.drain() {
                drop(Box::from_raw(s));
            }
            for (_, p) in self.pictures.drain() {
                let (b, cap) = ((*p).slices, (*p).slices_cap);
                if cap > 0 {
                    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(b, cap)));
                }
                drop(Box::from_raw(p));
            }
            for (b, cap) in self.retired.drain(..) {
                drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(b, cap)));
            }
        }
    }
}

fn s_type_of(p: *const core::ffi::c_void) -> Option<i32> {
    // SAFETY (caller): every `pNext` link begins with `sType`.
    (!p.is_null()).then(|| unsafe { *(p as *const i32) })
}

impl Session {
    fn slot(&mut self, index: i32) -> *mut SlotStore {
        *self.slots.entry(index).or_insert_with(zeroed_box)
    }

    /// Copy one raised reference slot into its store; the element to hand the
    /// driver instead, pointing into the store. `None` for a chain this does
    /// not know.
    ///
    /// # Safety
    /// Every pointer in `e` is valid (it was raised for this call).
    unsafe fn stabilise(&mut self, e: &VkVideoReferenceSlotInfoKHR) -> Option<*const VkVideoReferenceSlotInfoKHR> {
        let dpb = match s_type_of(e.p_next) {
            None => None,
            Some(H264_DPB_SLOT) => {
                let d = unsafe { *(e.p_next as *const VkVideoDecodeH264DpbSlotInfoKHR) };
                if !d.p_next.is_null() {
                    return None;
                }
                Some(d)
            }
            Some(_) => return None,
        };
        let pic = (!e.p_picture_resource.is_null()).then(|| unsafe { *e.p_picture_resource });
        if pic.is_some_and(|p| !p.p_next.is_null()) {
            return None;
        }
        let s = self.slot(e.slot_index);
        // SAFETY: `s` is this session's allocation; written through the raw
        // pointer only.
        unsafe {
            (*s).slot = *e;
            (*s).slot.p_next = std::ptr::null();
            (*s).slot.p_picture_resource = std::ptr::null();
            if let Some(p) = pic {
                (*s).picture = p;
                (*s).slot.p_picture_resource = &raw const (*s).picture;
            }
            if let Some(d) = dpb {
                (*s).h264 = d;
                (*s).h264.p_std_reference_info = std::ptr::null();
                if !d.p_std_reference_info.is_null() {
                    (*s).h264_std = *d.p_std_reference_info;
                    (*s).h264.p_std_reference_info = &raw const (*s).h264_std;
                }
                (*s).slot.p_next = (&raw const (*s).h264).cast();
            }
            Some(&raw const (*s).slot)
        }
    }
}

impl Stores {
    /// A `vkCmdBeginVideoCodingKHR` as raised: record its session for the
    /// command buffer, and point each reference slot element at its store.
    /// The array itself is the call's own, as ffmpeg's is on its stack.
    ///
    /// # Safety
    /// `info` and everything it points to are valid and owned by this call's
    /// raise pool, and its `pReferenceSlots` array may be written.
    pub unsafe fn begin(&mut self, cb: u64, info: *const VkVideoBeginCodingInfoKHR) {
        let Some(info) = (unsafe { info.as_ref() }) else { return };
        let session = info.video_session as u64;
        self.scope.insert(cb, session);
        let s = self.sessions.entry(session).or_default();
        let n = info.reference_slot_count as usize;
        if n == 0 || info.p_reference_slots.is_null() {
            return;
        }
        let elems = info.p_reference_slots as *mut VkVideoReferenceSlotInfoKHR;
        for i in 0..n {
            // SAFETY: the array has `n` elements in this call's pool.
            let e = unsafe { *elems.add(i) };
            if let Some(stable) = unsafe { s.stabilise(&e) } {
                unsafe { *elems.add(i) = *stable };
            }
        }
    }

    /// A `vkCmdDecodeVideoKHR` as raised: the decode info to hand the driver
    /// instead, in the store of its setup slot, with every slot pointing into
    /// the slot stores. `None` leaves the call as raised.
    ///
    /// # Safety
    /// `info` and everything it points to are valid for this call.
    pub unsafe fn decode(&mut self, cb: u64, info: *const VkVideoDecodeInfoKHR) -> Option<*const VkVideoDecodeInfoKHR> {
        let info = unsafe { info.as_ref() }?;
        let session = *self.scope.get(&cb)?;
        let s = self.sessions.entry(session).or_default();
        // The picture info chain: H.264 and nothing behind it.
        if s_type_of(info.p_next) != Some(H264_PICTURE) {
            return None;
        }
        let pic = unsafe { *(info.p_next as *const VkVideoDecodeH264PictureInfoKHR) };
        if !pic.p_next.is_null() || !info.dst_picture_resource.p_next.is_null() {
            return None;
        }
        let n = info.reference_slot_count as usize;
        if n > MAX_REFS || (n > 0 && info.p_reference_slots.is_null()) {
            return None;
        }
        // Slots first: the setup slot and every reference.
        let setup = match unsafe { info.p_setup_reference_slot.as_ref() } {
            Some(e) => Some((e.slot_index, unsafe { s.stabilise(e) }?)),
            None => None,
        };
        let mut refs: Vec<VkVideoReferenceSlotInfoKHR> = Vec::with_capacity(n);
        for i in 0..n {
            let e = unsafe { *info.p_reference_slots.add(i) };
            let stable = unsafe { s.stabilise(&e) }?;
            refs.push(unsafe { *stable });
        }
        let key = setup.map_or(i32::MIN, |(i, _)| i);
        let p = *s.pictures.entry(key).or_insert_with(zeroed_box);
        let count = pic.slice_count as usize;
        // SAFETY: `p` is this session's allocation, written through the raw
        // pointer only; `pic`'s pointers are valid for this call.
        unsafe {
            for (i, r) in refs.iter().enumerate() {
                (*p).refs[i] = *r;
            }
            if count > (*p).slices_cap {
                if (*p).slices_cap > 0 {
                    s.retired.push(((*p).slices, (*p).slices_cap));
                }
                let cap = count.max(64);
                (*p).slices = slice_buffer(cap);
                (*p).slices_cap = cap;
            }
            if count > 0 && !pic.p_slice_offsets.is_null() {
                std::ptr::copy_nonoverlapping(pic.p_slice_offsets, (*p).slices, count);
            }
            (*p).h264 = pic;
            (*p).h264.p_std_picture_info = std::ptr::null();
            if !pic.p_std_picture_info.is_null() {
                (*p).h264_std = *pic.p_std_picture_info;
                (*p).h264.p_std_picture_info = &raw const (*p).h264_std;
            }
            (*p).h264.p_slice_offsets = if count > 0 { (*p).slices } else { std::ptr::null() };
            (*p).info = *info;
            (*p).info.p_next = (&raw const (*p).h264).cast();
            (*p).info.p_setup_reference_slot = setup.map_or(std::ptr::null(), |(_, ptr)| ptr);
            (*p).info.p_reference_slots =
                if n > 0 { (&raw const (*p).refs).cast() } else { std::ptr::null() };
            Some(&raw const (*p).info)
        }
    }

    /// The command buffer's scope ended (`vkCmdEndVideoCodingKHR`).
    pub fn end(&mut self, cb: u64) {
        self.scope.remove(&cb);
    }

    /// Slot and picture stores, for diagnostics and tests.
    pub fn counts(&self) -> (usize, usize) {
        self.sessions.values().fold((0, 0), |(a, b), s| (a + s.slots.len(), b + s.pictures.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zeroed<T>() -> T {
        // SAFETY: test C structures; all-zero is valid.
        unsafe { std::mem::zeroed() }
    }

    struct Raised {
        std: StdVideoDecodeH264ReferenceInfo,
        dpb: VkVideoDecodeH264DpbSlotInfoKHR,
        pic: VkVideoPictureResourceInfoKHR,
        slot: VkVideoReferenceSlotInfoKHR,
    }

    /// A raised reference slot with its chain, as a raise pool would own it:
    /// through a raw pointer, freed with [`free`] (a `Box` would be retagged
    /// on drop, under the pointers into it).
    fn raised(index: i32, view: u64, frame_num: u16) -> *mut Raised {
        let r = Box::into_raw(Box::new(Raised { std: zeroed(), dpb: zeroed(), pic: zeroed(), slot: zeroed() }));
        // SAFETY: `r` is a fresh allocation this function owns.
        unsafe {
            (*r).std.frame_num = frame_num;
            (*r).dpb.s_type = VkStructureType(H264_DPB_SLOT);
            (*r).dpb.p_std_reference_info = &raw const (*r).std;
            (*r).pic.image_view_binding = view as _;
            (*r).slot.slot_index = index;
            (*r).slot.p_next = (&raw const (*r).dpb).cast();
            (*r).slot.p_picture_resource = &raw const (*r).pic;
        }
        r
    }

    fn free(r: *mut Raised) {
        // SAFETY: from `raised`, freed once.
        drop(unsafe { Box::from_raw(r) });
    }

    /// The fault's shape: the driver keeps a pointer from an earlier decode,
    /// the raised copies are freed, and a later IDR follows the pointer. With
    /// the stores, the kept pointer still reads that slot's picture.
    #[test]
    fn a_pointer_kept_from_an_earlier_decode_still_reads_its_slot() {
        let mut st = Stores::default();
        let cb = 7u64;
        let mut begin: VkVideoBeginCodingInfoKHR = zeroed();
        begin.video_session = 0x55 as _;
        unsafe { st.begin(cb, &begin) };

        let mut std_pic: StdVideoDecodeH264PictureInfo = zeroed();
        std_pic.frame_num = 3;
        let offsets = [0u32];
        let mut pic: VkVideoDecodeH264PictureInfoKHR = zeroed();
        pic.s_type = VkStructureType(H264_PICTURE);
        pic.p_std_picture_info = &raw const std_pic;
        pic.slice_count = 1;
        pic.p_slice_offsets = offsets.as_ptr();
        let setup = raised(1, 0xA1, 3);
        let reference = raised(0, 0xA0, 2);
        let mut info: VkVideoDecodeInfoKHR = zeroed();
        info.p_next = (&raw const pic).cast();
        info.p_setup_reference_slot = unsafe { &raw const (*setup).slot };
        info.reference_slot_count = 1;
        info.p_reference_slots = unsafe { &raw const (*reference).slot };

        let stable = unsafe { st.decode(cb, &info) }.expect("an H.264 decode is stabilised");
        // What a driver might keep: the setup slot's picture resource.
        let kept = unsafe { (*(*stable).p_setup_reference_slot).p_picture_resource };
        let kept_std = unsafe {
            let d = (*(*stable).p_setup_reference_slot).p_next as *const VkVideoDecodeH264DpbSlotInfoKHR;
            (*d).p_std_reference_info
        };
        free(setup);
        free(reference);
        assert_eq!(unsafe { (*kept).image_view_binding } as u64, 0xA1);
        assert_eq!(unsafe { (*kept_std).frame_num }, 3);
        assert_eq!(unsafe { *(*stable).p_reference_slots }.slot_index, 0);

        // The next picture set up in slot 1 overwrites the same memory, as
        // ffmpeg's recycled per-picture memory does.
        let again = raised(1, 0xB1, 9);
        info.p_setup_reference_slot = unsafe { &raw const (*again).slot };
        info.reference_slot_count = 0;
        let stable2 = unsafe { st.decode(cb, &info) }.unwrap();
        assert_eq!(stable2, stable, "one decode store per setup slot");
        assert_eq!(unsafe { (*kept).image_view_binding } as u64, 0xB1);
        assert_eq!(unsafe { (*kept_std).frame_num }, 9);
        assert_eq!(st.counts(), (2, 1));
        free(again);
    }

    /// A begin's elements are pointed at the same slot stores a decode uses.
    #[test]
    fn a_begins_slots_share_the_decodes_stores() {
        let mut st = Stores::default();
        let r = raised(4, 0xC4, 1);
        let mut elems = [unsafe { (*r).slot }];
        let mut begin: VkVideoBeginCodingInfoKHR = zeroed();
        begin.video_session = 0x66 as _;
        begin.reference_slot_count = 1;
        begin.p_reference_slots = elems.as_mut_ptr();
        unsafe { st.begin(9, &begin) };
        let p = elems[0].p_picture_resource;
        assert_ne!(p, unsafe { &raw const (*r).pic }, "the element now points into the store");
        free(r);
        assert_eq!(unsafe { (*p).image_view_binding } as u64, 0xC4);
    }

    /// Another codec's chain is left exactly as raised.
    #[test]
    fn an_unknown_chain_is_left_alone() {
        let mut st = Stores::default();
        let mut begin: VkVideoBeginCodingInfoKHR = zeroed();
        begin.video_session = 1 as _;
        unsafe { st.begin(1, &begin) };
        let mut other: VkVideoDecodeH264PictureInfoKHR = zeroed();
        other.s_type = VkStructureType(1_000_512_001); // an AV1 picture info
        let mut info: VkVideoDecodeInfoKHR = zeroed();
        info.p_next = (&raw const other).cast();
        assert!(unsafe { st.decode(1, &info) }.is_none());
        // And a decode outside any scope this side saw.
        assert!(unsafe { st.decode(2, &info) }.is_none());
    }
}
