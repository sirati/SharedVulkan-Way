//! The C structures of a video decode, at addresses that stay valid for the
//! session: the fix for Intel's decode fault (docs/video-decode.md §7.11.5),
//! for every codec this side serves there (§7.12).
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
//! per-picture private data (`H264VulkanDecodePicture`,
//! `HEVCVulkanDecodePicture`, `AV1VulkanDecodePicture`: the picture resource,
//! the setup slot, the codec's DPB slot info, the decode info and its
//! reference array, and for AV1 the tile info with its four arrays and the
//! quantization, segmentation, loop filter, CDEF, loop restoration, global
//! motion and film grain blocks). That memory comes from a buffer pool and is
//! recycled, never freed, so a stale pointer always reads *some* picture's
//! structures of the same type. Its H.265 parameter sets live in one buffer for
//! the decoder's whole life (`FFVulkanDecodeContext::hevc_headers`). The
//! forwarder raised fresh copies for each call and freed them when the call
//! returned.
//!
//! # What it does
//!
//! Per video session there is one [`SlotStore`] per DPB slot index (and one for
//! index -1, a begin's not-yet-active picture). It holds that slot's
//! `VkVideoReferenceSlotInfoKHR`, `VkVideoPictureResourceInfoKHR`, and the
//! codec's DPB slot info and std reference info. There is one [`PictureStore`]
//! per setup slot (and one for a decode with none). It holds the
//! `VkVideoDecodeInfoKHR`, the codec's picture info and everything it points
//! to, and the reference array. Each is boxed and never moves while the session
//! lives. Every call copies what it was given into the store for its slot and
//! hands the driver pointers into the stores. A pointer the driver kept
//! therefore always reads the structures of the picture now in that slot, as
//! natively.
//!
//! The raise pools of `vkCreateVideoSessionParametersKHR` and
//! `vkUpdateVideoSessionParametersKHR` (the SPS, PPS, VPS and AV1 sequence
//! header arrays) are kept with their session too ([`Stores::keep_params`]).
//!
//! The begin's reference *array* is per call, as natively: ffmpeg builds it on
//! the stack. Its elements point into the stores.
//!
//! A session's stores are freed when it is destroyed, which the specification
//! allows only once no command naming it is pending; the rest when the
//! connection ends.
//!
//! # The slot guard
//!
//! A slot index at or beyond the session's `maxDpbSlots`, or more references
//! than its `maxActiveReferencePictures`, is refused here and never reaches the
//! driver: the command buffer is poisoned, its submit fails, and the
//! application decodes in software (Firefox on Linux does so after any decode
//! error of a hardware decoder). ffmpeg 9.0.1's AV1 decoder uses slot index 8
//! on a driver reporting 8 slots, as Intel's does for AV1 (§7.12). That is a
//! spec violation no local driver punishes and nobody has measured on Intel.
//!
//! H.264, H.265 and AV1. A chain this does not know (VP9's picture info, an
//! inline-parameters link) leaves the call exactly as raised, and
//! [`crate::driver`] does not serve those codecs on Intel's Windows driver.

use std::collections::HashMap;
use sv_gpu_entry::*;

/// `VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_DPB_SLOT_INFO_KHR`.
const H264_DPB_SLOT: i32 = 1_000_040_006;
/// `VK_STRUCTURE_TYPE_VIDEO_DECODE_H264_PICTURE_INFO_KHR`.
const H264_PICTURE: i32 = 1_000_040_001;
/// `VK_STRUCTURE_TYPE_VIDEO_DECODE_H265_DPB_SLOT_INFO_KHR`.
const H265_DPB_SLOT: i32 = 1_000_187_005;
/// `VK_STRUCTURE_TYPE_VIDEO_DECODE_H265_PICTURE_INFO_KHR`.
const H265_PICTURE: i32 = 1_000_187_004;
/// `VK_STRUCTURE_TYPE_VIDEO_DECODE_AV1_DPB_SLOT_INFO_KHR`.
const AV1_DPB_SLOT: i32 = 1_000_512_005;
/// `VK_STRUCTURE_TYPE_VIDEO_DECODE_AV1_PICTURE_INFO_KHR`.
const AV1_PICTURE: i32 = 1_000_512_001;

/// The most reference slots one call may name, with room to spare: H.264 has
/// 16 DPB slots plus the setup one.
const MAX_REFS: usize = 33;

/// `STD_VIDEO_AV1_MAX_TILE_COLS` and `_ROWS`: the length of each tile array.
const AV1_MAX_TILES: usize = 64;

/// The structures of one DPB slot. Only the block of the codec in use is ever
/// written; the rest stay zero.
#[repr(C)]
pub struct SlotStore {
    slot: VkVideoReferenceSlotInfoKHR,
    picture: VkVideoPictureResourceInfoKHR,
    h264: VkVideoDecodeH264DpbSlotInfoKHR,
    h264_std: StdVideoDecodeH264ReferenceInfo,
    h265: VkVideoDecodeH265DpbSlotInfoKHR,
    h265_std: StdVideoDecodeH265ReferenceInfo,
    av1: VkVideoDecodeAV1DpbSlotInfoKHR,
    av1_std: StdVideoDecodeAV1ReferenceInfo,
}

/// AV1's picture structures: `StdVideoDecodeAV1PictureInfo`'s eight pointers
/// and the tile info's four arrays, laid out as ffmpeg's
/// `AV1VulkanDecodePicture` holds them.
#[repr(C)]
struct Av1Picture {
    info: VkVideoDecodeAV1PictureInfoKHR,
    std: StdVideoDecodeAV1PictureInfo,
    tile: StdVideoAV1TileInfo,
    mi_col_starts: [u16; AV1_MAX_TILES],
    mi_row_starts: [u16; AV1_MAX_TILES],
    width_in_sbs_minus1: [u16; AV1_MAX_TILES],
    height_in_sbs_minus1: [u16; AV1_MAX_TILES],
    quantization: StdVideoAV1Quantization,
    segmentation: StdVideoAV1Segmentation,
    loop_filter: StdVideoAV1LoopFilter,
    cdef: StdVideoAV1CDEF,
    loop_restoration: StdVideoAV1LoopRestoration,
    global_motion: StdVideoAV1GlobalMotion,
    film_grain: StdVideoAV1FilmGrain,
}

/// A `u32` array the driver may hold the address of: never reallocated in
/// place. A larger count gets a new buffer, and the old one is kept in the
/// session's `retired` list.
#[repr(C)]
struct Words {
    ptr: *mut u32,
    cap: usize,
}

/// The structures of one decode, by its setup slot.
#[repr(C)]
pub struct PictureStore {
    info: VkVideoDecodeInfoKHR,
    h264: VkVideoDecodeH264PictureInfoKHR,
    h264_std: StdVideoDecodeH264PictureInfo,
    h265: VkVideoDecodeH265PictureInfoKHR,
    h265_std: StdVideoDecodeH265PictureInfo,
    av1: Av1Picture,
    refs: [VkVideoReferenceSlotInfoKHR; MAX_REFS],
    /// H.264 slice offsets, H.265 slice segment offsets or AV1 tile offsets.
    offsets: Words,
    /// AV1 tile sizes.
    sizes: Words,
}

/// Why a call is not handed to the driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The `poison_command_buffer` reason: completes "the driver has no …".
    pub why: &'static str,
    /// The numbers, for the one stderr line.
    pub detail: String,
}

/// What [`Stores::decode`] makes of a call.
#[derive(Debug, PartialEq, Eq)]
pub enum Decode {
    /// Hand the driver this, in the stores.
    Stable(*const VkVideoDecodeInfoKHR),
    /// A chain the stores do not know: forward the call as raised.
    Raised,
    /// Do not record it (the slot guard).
    Refused(Refusal),
}

/// Every video session's stores, for one connection.
#[derive(Default)]
pub struct Stores {
    sessions: HashMap<u64, Session>,
    /// The session each command buffer's open coding scope belongs to, from
    /// its `vkCmdBeginVideoCodingKHR`.
    scope: HashMap<u64, u64>,
    /// Each parameters object's session, from its creation.
    params: HashMap<u64, u64>,
    /// The session whose parameters call just returned, per thread, waiting
    /// for that call's raise pool ([`Stores::keep_params`]).
    pending: HashMap<std::thread::ThreadId, u64>,
}

/// One session's stores. Each is a `Box::into_raw` allocation that is only
/// ever written through that raw pointer, never through a `&mut`, so the
/// pointers the driver keeps stay valid in Rust's aliasing model as well as in
/// fact; all are freed when the session is destroyed or the connection ends.
#[derive(Default)]
struct Session {
    slots: HashMap<i32, *mut SlotStore>,
    pictures: HashMap<i32, *mut PictureStore>,
    retired: Vec<(*mut u32, usize)>,
    /// The raise pools of the session's parameters calls.
    params: Vec<Raise>,
    /// `maxDpbSlots` and `maxActiveReferencePictures` from its creation, if
    /// this side saw it.
    limits: Option<(u32, u32)>,
}

// SAFETY: the raw pointers point only into the stores' own allocations or the
// kept raise pools (or are copied driver handles); all access goes through the
// `Mutex` in `server::Shared`.
unsafe impl Send for Stores {}

fn zeroed_box<T>() -> *mut T {
    // SAFETY: only used for the plain C structures above, for which all-zero
    // is a valid (empty) value.
    Box::into_raw(Box::new(unsafe { std::mem::zeroed::<T>() }))
}

fn word_buffer(cap: usize) -> *mut u32 {
    Box::into_raw(vec![0u32; cap].into_boxed_slice()) as *mut u32
}

fn free_words(b: *mut u32, cap: usize) {
    if cap > 0 {
        // SAFETY: from `word_buffer(cap)`, freed once.
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(b, cap)) });
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: every pointer came from `Box::into_raw` here and is freed
        // once; the driver may no longer read them (the session is destroyed,
        // or the connection's device is gone).
        unsafe {
            for (_, s) in self.slots.drain() {
                drop(Box::from_raw(s));
            }
            for (_, p) in self.pictures.drain() {
                free_words((*p).offsets.ptr, (*p).offsets.cap);
                free_words((*p).sizes.ptr, (*p).sizes.cap);
                drop(Box::from_raw(p));
            }
        }
        for (b, cap) in self.retired.drain(..) {
            free_words(b, cap);
        }
    }
}

fn s_type_of(p: *const core::ffi::c_void) -> Option<i32> {
    // SAFETY (caller): every `pNext` link begins with `sType`.
    (!p.is_null()).then(|| unsafe { *(p as *const i32) })
}

/// Copy `*src` into `*dst` and point `*field` at `dst`, or null `*field` for a
/// null `src`.
///
/// # Safety
/// `src` is null or valid; `dst` and `field` are valid for writes.
unsafe fn keep<T: Copy>(src: *const T, dst: *mut T, field: *mut *const T) {
    unsafe {
        if src.is_null() {
            *field = std::ptr::null();
        } else {
            *dst = *src;
            *field = dst;
        }
    }
}

/// Copy at most `cap` of `n` elements from `src` (if it is not null) to `dst`;
/// the pointer to hand the driver.
///
/// # Safety
/// `src` is null or valid for `n` reads; `dst` for `cap` writes.
unsafe fn keep_array(src: *const u16, n: usize, dst: *mut u16, cap: usize) -> *const u16 {
    if src.is_null() {
        return std::ptr::null();
    }
    // SAFETY: the caller's.
    unsafe { std::ptr::copy_nonoverlapping(src, dst, n.min(cap)) };
    dst
}

/// The slot indices a decode or begin names, and how many references.
fn check(limits: Option<(u32, u32)>, what: &str, indices: &[i32], refs: usize) -> Result<(), Refusal> {
    let Some((slots, active)) = limits else { return Ok(()) };
    if let Some(&i) = indices.iter().find(|&&i| i >= 0 && i as u32 >= slots) {
        return Err(Refusal {
            why: "DPB slot for a slotIndex at or beyond the session's maxDpbSlots",
            detail: format!("{what} names slotIndex {i}, and the session has {slots} DPB slots"),
        });
    }
    if refs as u32 > active {
        return Err(Refusal {
            why: "room for more references than the session's maxActiveReferencePictures",
            detail: format!("{what} names {refs} references, and the session allows {active}"),
        });
    }
    Ok(())
}

impl Session {
    fn slot(&mut self, index: i32) -> *mut SlotStore {
        *self.slots.entry(index).or_insert_with(zeroed_box)
    }

    /// Grow `w` to hold `n` words, retiring the old buffer.
    ///
    /// # Safety
    /// `w` is a field of one of this session's picture stores.
    unsafe fn grow(&mut self, w: *mut Words, n: usize) {
        unsafe {
            if n > (*w).cap {
                if (*w).cap > 0 {
                    self.retired.push(((*w).ptr, (*w).cap));
                }
                let cap = n.max(64);
                (*w).ptr = word_buffer(cap);
                (*w).cap = cap;
            }
        }
    }

    /// Copy `n` words from `src` into `w`; the pointer to hand the driver.
    ///
    /// # Safety
    /// As [`Session::grow`]; `src` is null or valid for `n` reads.
    unsafe fn words(&mut self, w: *mut Words, src: *const u32, n: usize) -> *const u32 {
        if n == 0 || src.is_null() {
            return std::ptr::null();
        }
        unsafe {
            self.grow(w, n);
            std::ptr::copy_nonoverlapping(src, (*w).ptr, n);
            (*w).ptr
        }
    }

    /// Copy one raised reference slot into its store; the element to hand the
    /// driver instead, pointing into the store. `None` for a chain this does
    /// not know.
    ///
    /// # Safety
    /// Every pointer in `e` is valid (it was raised for this call).
    unsafe fn stabilise(&mut self, e: &VkVideoReferenceSlotInfoKHR) -> Option<*const VkVideoReferenceSlotInfoKHR> {
        let codec = s_type_of(e.p_next);
        if !matches!(codec, None | Some(H264_DPB_SLOT | H265_DPB_SLOT | AV1_DPB_SLOT)) {
            return None;
        }
        // Each codec's DPB slot info has the same head: sType, pNext,
        // pStdReferenceInfo. Nothing may follow it.
        if codec.is_some() && !unsafe { (*(e.p_next as *const VkVideoDecodeH264DpbSlotInfoKHR)).p_next }.is_null() {
            return None;
        }
        let pic = (!e.p_picture_resource.is_null()).then(|| unsafe { *e.p_picture_resource });
        if pic.is_some_and(|p| !p.p_next.is_null()) {
            return None;
        }
        let s = self.slot(e.slot_index);
        // SAFETY: `s` is this session's allocation, written through the raw
        // pointer only; `e`'s chain is valid and its `sType` says what it is.
        unsafe {
            (*s).slot = *e;
            (*s).slot.p_next = std::ptr::null();
            (*s).slot.p_picture_resource = std::ptr::null();
            if let Some(p) = pic {
                (*s).picture = p;
                (*s).slot.p_picture_resource = &raw const (*s).picture;
            }
            match codec {
                Some(H264_DPB_SLOT) => {
                    let d = e.p_next as *const VkVideoDecodeH264DpbSlotInfoKHR;
                    (*s).h264 = *d;
                    keep((*d).p_std_reference_info, &raw mut (*s).h264_std, &raw mut (*s).h264.p_std_reference_info);
                    (*s).slot.p_next = (&raw const (*s).h264).cast();
                }
                Some(H265_DPB_SLOT) => {
                    let d = e.p_next as *const VkVideoDecodeH265DpbSlotInfoKHR;
                    (*s).h265 = *d;
                    keep((*d).p_std_reference_info, &raw mut (*s).h265_std, &raw mut (*s).h265.p_std_reference_info);
                    (*s).slot.p_next = (&raw const (*s).h265).cast();
                }
                Some(AV1_DPB_SLOT) => {
                    let d = e.p_next as *const VkVideoDecodeAV1DpbSlotInfoKHR;
                    (*s).av1 = *d;
                    keep((*d).p_std_reference_info, &raw mut (*s).av1_std, &raw mut (*s).av1.p_std_reference_info);
                    (*s).slot.p_next = (&raw const (*s).av1).cast();
                }
                _ => {}
            }
            Some(&raw const (*s).slot)
        }
    }

    /// Copy an H.265 picture info into `p`; its address there.
    ///
    /// # Safety
    /// `p` is one of this session's picture stores; `h`'s pointers are valid.
    unsafe fn h265(&mut self, p: *mut PictureStore, h: &VkVideoDecodeH265PictureInfoKHR) -> *const core::ffi::c_void {
        unsafe {
            (*p).h265 = *h;
            keep(h.p_std_picture_info, &raw mut (*p).h265_std, &raw mut (*p).h265.p_std_picture_info);
            (*p).h265.p_slice_segment_offsets =
                self.words(&raw mut (*p).offsets, h.p_slice_segment_offsets, h.slice_segment_count as usize);
            (&raw const (*p).h265).cast()
        }
    }

    /// Copy an H.264 picture info into `p`; its address there.
    ///
    /// # Safety
    /// As [`Session::h265`].
    unsafe fn h264(&mut self, p: *mut PictureStore, h: &VkVideoDecodeH264PictureInfoKHR) -> *const core::ffi::c_void {
        unsafe {
            (*p).h264 = *h;
            keep(h.p_std_picture_info, &raw mut (*p).h264_std, &raw mut (*p).h264.p_std_picture_info);
            (*p).h264.p_slice_offsets = self.words(&raw mut (*p).offsets, h.p_slice_offsets, h.slice_count as usize);
            (&raw const (*p).h264).cast()
        }
    }

    /// Copy an AV1 picture info, its std picture info and everything that
    /// points to, into `p`; its address there.
    ///
    /// # Safety
    /// As [`Session::h265`].
    unsafe fn av1(&mut self, p: *mut PictureStore, h: &VkVideoDecodeAV1PictureInfoKHR) -> *const core::ffi::c_void {
        unsafe {
            let a = &raw mut (*p).av1;
            (*a).info = *h;
            let n = h.tile_count as usize;
            (*a).info.p_tile_offsets = self.words(&raw mut (*p).offsets, h.p_tile_offsets, n);
            (*a).info.p_tile_sizes = self.words(&raw mut (*p).sizes, h.p_tile_sizes, n);
            keep(h.p_std_picture_info, &raw mut (*a).std, &raw mut (*a).info.p_std_picture_info);
            if let Some(std) = h.p_std_picture_info.as_ref() {
                let s = &raw mut (*a).std;
                keep(std.p_tile_info, &raw mut (*a).tile, &raw mut (*s).p_tile_info);
                if let Some(t) = std.p_tile_info.as_ref() {
                    let (cols, rows) = (t.tile_cols as usize, t.tile_rows as usize);
                    let tile = &raw mut (*a).tile;
                    (*tile).p_mi_col_starts =
                        keep_array(t.p_mi_col_starts, cols, (&raw mut (*a).mi_col_starts).cast(), AV1_MAX_TILES);
                    (*tile).p_mi_row_starts =
                        keep_array(t.p_mi_row_starts, rows, (&raw mut (*a).mi_row_starts).cast(), AV1_MAX_TILES);
                    (*tile).p_width_in_sbs_minus1 =
                        keep_array(t.p_width_in_sbs_minus1, cols, (&raw mut (*a).width_in_sbs_minus1).cast(), AV1_MAX_TILES);
                    (*tile).p_height_in_sbs_minus1 =
                        keep_array(t.p_height_in_sbs_minus1, rows, (&raw mut (*a).height_in_sbs_minus1).cast(), AV1_MAX_TILES);
                }
                keep(std.p_quantization, &raw mut (*a).quantization, &raw mut (*s).p_quantization);
                keep(std.p_segmentation, &raw mut (*a).segmentation, &raw mut (*s).p_segmentation);
                keep(std.p_loop_filter, &raw mut (*a).loop_filter, &raw mut (*s).p_loop_filter);
                keep(std.p_cdef, &raw mut (*a).cdef, &raw mut (*s).p_cdef);
                keep(std.p_loop_restoration, &raw mut (*a).loop_restoration, &raw mut (*s).p_loop_restoration);
                keep(std.p_global_motion, &raw mut (*a).global_motion, &raw mut (*s).p_global_motion);
                keep(std.p_film_grain, &raw mut (*a).film_grain, &raw mut (*s).p_film_grain);
            }
            (&raw const (*a).info).cast()
        }
    }
}

impl Stores {
    /// A `vkCreateVideoSessionKHR` returned this session, created with these
    /// limits. A handle value seen before names a new session now.
    pub fn session_created(&mut self, session: u64, max_dpb_slots: u32, max_active_refs: u32) {
        let mut s = Session::default();
        s.limits = Some((max_dpb_slots, max_active_refs));
        self.sessions.insert(session, s);
    }

    /// A `vkDestroyVideoSessionKHR`: the driver no longer reads anything of
    /// the session's, so its stores and parameters pools are freed.
    pub fn session_destroyed(&mut self, session: u64) {
        self.sessions.remove(&session);
        self.scope.retain(|_, s| *s != session);
        self.params.retain(|_, s| *s != session);
    }

    /// A `vkCreateVideoSessionParametersKHR` for `session` returned `params`
    /// (null if it failed); its raise pool comes next, to [`Stores::keep_params`].
    pub fn params_created(&mut self, params: u64, session: u64) {
        if params != 0 {
            self.params.insert(params, session);
        }
        self.pending.insert(std::thread::current().id(), session);
    }

    /// A `vkUpdateVideoSessionParametersKHR` of `params` returned; its raise
    /// pool comes next.
    pub fn params_updated(&mut self, params: u64) {
        if let Some(&s) = self.params.get(&params) {
            self.pending.insert(std::thread::current().id(), s);
        }
    }

    /// The raise pool of the parameters call this thread just made: kept with
    /// its session, as ffmpeg keeps its parameter-set arrays for the
    /// decoder's life. Dropped if no session claims it.
    pub fn keep_params(&mut self, pool: Raise) {
        let Some(s) = self.pending.remove(&std::thread::current().id()) else { return };
        if let Some(session) = self.sessions.get_mut(&s) {
            session.params.push(pool);
        } else {
            // A session this side never saw created: keep the pool anyway, in
            // a session entry without limits, freed with the connection.
            self.sessions.entry(s).or_default().params.push(pool);
        }
    }

    /// A `vkDestroyVideoSessionParametersKHR`. Its pools stay with the
    /// session.
    pub fn params_destroyed(&mut self, params: u64) {
        self.params.remove(&params);
    }

    /// A `vkCmdBeginVideoCodingKHR` as raised: record its session for the
    /// command buffer, and point each reference slot element at its store.
    /// The array itself is the call's own, as ffmpeg's is on its stack.
    ///
    /// # Safety
    /// `info` and everything it points to are valid and owned by this call's
    /// raise pool, and its `pReferenceSlots` array may be written.
    pub unsafe fn begin(&mut self, cb: u64, info: *const VkVideoBeginCodingInfoKHR) -> Result<(), Refusal> {
        let Some(info) = (unsafe { info.as_ref() }) else { return Ok(()) };
        let session = info.video_session;
        let s = self.sessions.entry(session).or_default();
        let n = info.reference_slot_count as usize;
        if n > 0 && !info.p_reference_slots.is_null() {
            let elems = info.p_reference_slots as *mut VkVideoReferenceSlotInfoKHR;
            // SAFETY: the array has `n` elements in this call's pool.
            let indices: Vec<i32> = (0..n).map(|i| unsafe { (*elems.add(i)).slot_index }).collect();
            // A begin's count includes the -1 element of the picture about to
            // be set up, so only the indices are checked here.
            check(s.limits, "vkCmdBeginVideoCodingKHR", &indices, 0)?;
            for i in 0..n {
                // SAFETY: as above.
                let e = unsafe { *elems.add(i) };
                if let Some(stable) = unsafe { s.stabilise(&e) } {
                    unsafe { *elems.add(i) = *stable };
                }
            }
        }
        self.scope.insert(cb, session);
        Ok(())
    }

    /// A `vkCmdDecodeVideoKHR` as raised: the decode info to hand the driver
    /// instead, in the store of its setup slot, with every slot pointing into
    /// the slot stores.
    ///
    /// # Safety
    /// `info` and everything it points to are valid for this call.
    pub unsafe fn decode(&mut self, cb: u64, info: *const VkVideoDecodeInfoKHR) -> Decode {
        let Some(info) = (unsafe { info.as_ref() }) else { return Decode::Raised };
        let Some(&session) = self.scope.get(&cb) else { return Decode::Raised };
        let s = self.sessions.entry(session).or_default();

        let n = info.reference_slot_count as usize;
        if n > 0 && info.p_reference_slots.is_null() {
            return Decode::Raised;
        }
        // SAFETY: a counted array of this call's.
        let raised_refs: &[VkVideoReferenceSlotInfoKHR] =
            if n == 0 { &[] } else { unsafe { std::slice::from_raw_parts(info.p_reference_slots, n) } };
        let setup_raised = unsafe { info.p_setup_reference_slot.as_ref() };
        let mut indices: Vec<i32> = raised_refs.iter().map(|r| r.slot_index).collect();
        indices.extend(setup_raised.map(|e| e.slot_index));
        if let Err(r) = check(s.limits, "vkCmdDecodeVideoKHR", &indices, n) {
            return Decode::Refused(r);
        }

        // The picture info chain: one codec's, and nothing behind it.
        let codec = s_type_of(info.p_next);
        if !matches!(codec, Some(H264_PICTURE | H265_PICTURE | AV1_PICTURE)) || n > MAX_REFS {
            return Decode::Raised;
        }
        // Each codec's picture info starts sType, pNext.
        if !unsafe { (*(info.p_next as *const VkVideoDecodeH264PictureInfoKHR)).p_next }.is_null()
            || !info.dst_picture_resource.p_next.is_null()
        {
            return Decode::Raised;
        }

        // Slots first: the setup slot and every reference.
        let setup = match setup_raised {
            Some(e) => match unsafe { s.stabilise(e) } {
                Some(p) => Some((e.slot_index, p)),
                None => return Decode::Raised,
            },
            None => None,
        };
        let mut refs: Vec<VkVideoReferenceSlotInfoKHR> = Vec::with_capacity(n);
        for e in raised_refs {
            match unsafe { s.stabilise(e) } {
                Some(stable) => refs.push(unsafe { *stable }),
                None => return Decode::Raised,
            }
        }
        let key = setup.map_or(i32::MIN, |(i, _)| i);
        let p = *s.pictures.entry(key).or_insert_with(zeroed_box);
        // SAFETY: `p` is this session's allocation, written through the raw
        // pointer only; the picture info's pointers are valid for this call
        // and its `sType` says what it is.
        unsafe {
            for (i, r) in refs.iter().enumerate() {
                (*p).refs[i] = *r;
            }
            let chain = match codec {
                Some(H264_PICTURE) => s.h264(p, &*(info.p_next as *const VkVideoDecodeH264PictureInfoKHR)),
                Some(H265_PICTURE) => s.h265(p, &*(info.p_next as *const VkVideoDecodeH265PictureInfoKHR)),
                _ => s.av1(p, &*(info.p_next as *const VkVideoDecodeAV1PictureInfoKHR)),
            };
            (*p).info = *info;
            (*p).info.p_next = chain;
            (*p).info.p_setup_reference_slot = setup.map_or(std::ptr::null(), |(_, ptr)| ptr);
            (*p).info.p_reference_slots = if n > 0 { (&raw const (*p).refs).cast() } else { std::ptr::null() };
            Decode::Stable(&raw const (*p).info)
        }
    }

    /// The command buffer's scope ended (`vkCmdEndVideoCodingKHR`). Whether
    /// this side saw it begun.
    pub fn end(&mut self, cb: u64) -> bool {
        self.scope.remove(&cb).is_some()
    }

    /// Slot and picture stores, for diagnostics and tests.
    pub fn counts(&self) -> (usize, usize) {
        self.sessions.values().fold((0, 0), |(a, b), s| (a + s.slots.len(), b + s.pictures.len()))
    }

    /// Kept parameters pools, for diagnostics and tests.
    pub fn params_pools(&self) -> usize {
        self.sessions.values().map(|s| s.params.len()).sum()
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

    fn free<T>(r: *mut T) {
        // SAFETY: from `Box::into_raw`, freed once.
        drop(unsafe { Box::from_raw(r) });
    }

    fn stable(d: Decode) -> *const VkVideoDecodeInfoKHR {
        match d {
            Decode::Stable(p) => p,
            other => panic!("expected a stabilised decode, got {other:?}"),
        }
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
        unsafe { st.begin(cb, &begin) }.unwrap();

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

        let stable1 = stable(unsafe { st.decode(cb, &info) });
        // What a driver might keep: the setup slot's picture resource.
        let kept = unsafe { (*(*stable1).p_setup_reference_slot).p_picture_resource };
        let kept_std = unsafe {
            let d = (*(*stable1).p_setup_reference_slot).p_next as *const VkVideoDecodeH264DpbSlotInfoKHR;
            (*d).p_std_reference_info
        };
        free(setup);
        free(reference);
        assert_eq!(unsafe { (*kept).image_view_binding }, 0xA1);
        assert_eq!(unsafe { (*kept_std).frame_num }, 3);
        assert_eq!(unsafe { *(*stable1).p_reference_slots }.slot_index, 0);

        // The next picture set up in slot 1 overwrites the same memory, as
        // ffmpeg's recycled per-picture memory does.
        let again = raised(1, 0xB1, 9);
        info.p_setup_reference_slot = unsafe { &raw const (*again).slot };
        info.reference_slot_count = 0;
        let stable2 = stable(unsafe { st.decode(cb, &info) });
        assert_eq!(stable2, stable1, "one decode store per setup slot");
        assert_eq!(unsafe { (*kept).image_view_binding }, 0xB1);
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
        unsafe { st.begin(9, &begin) }.unwrap();
        let p = elems[0].p_picture_resource;
        assert_ne!(p, unsafe { &raw const (*r).pic }, "the element now points into the store");
        free(r);
        assert_eq!(unsafe { (*p).image_view_binding }, 0xC4);
    }

    /// A chain the stores do not know is left exactly as raised.
    #[test]
    fn an_unknown_chain_is_left_alone() {
        let mut st = Stores::default();
        let mut begin: VkVideoBeginCodingInfoKHR = zeroed();
        begin.video_session = 1 as _;
        unsafe { st.begin(1, &begin) }.unwrap();
        let mut other: VkVideoDecodeH264PictureInfoKHR = zeroed();
        other.s_type = VkStructureType(1_000_514_002); // a VP9 picture info
        let mut info: VkVideoDecodeInfoKHR = zeroed();
        info.p_next = (&raw const other).cast();
        assert_eq!(unsafe { st.decode(1, &info) }, Decode::Raised);
        // And a decode outside any scope this side saw.
        assert_eq!(unsafe { st.decode(2, &info) }, Decode::Raised);
    }

    /// H.265: the DPB slot info, the std reference info, the picture info, the
    /// std picture info and the slice segment offsets all outlive the raised
    /// copies.
    #[test]
    fn h265_structures_outlive_the_call() {
        let mut st = Stores::default();
        let mut begin: VkVideoBeginCodingInfoKHR = zeroed();
        begin.video_session = 0x265 as _;
        unsafe { st.begin(3, &begin) }.unwrap();

        let std_ref = Box::into_raw(Box::new(zeroed::<StdVideoDecodeH265ReferenceInfo>()));
        let dpb = Box::into_raw(Box::new(zeroed::<VkVideoDecodeH265DpbSlotInfoKHR>()));
        let res = Box::into_raw(Box::new(zeroed::<VkVideoPictureResourceInfoKHR>()));
        let slot = Box::into_raw(Box::new(zeroed::<VkVideoReferenceSlotInfoKHR>()));
        let std_pic = Box::into_raw(Box::new(zeroed::<StdVideoDecodeH265PictureInfo>()));
        let offsets = Box::into_raw(Box::new([0u32, 777]));
        let pic = Box::into_raw(Box::new(zeroed::<VkVideoDecodeH265PictureInfoKHR>()));
        let info = Box::into_raw(Box::new(zeroed::<VkVideoDecodeInfoKHR>()));
        // SAFETY: fresh allocations this test owns.
        let stable1 = unsafe {
            (*std_ref).pic_order_cnt_val = 42;
            (*dpb).s_type = VkStructureType(H265_DPB_SLOT);
            (*dpb).p_std_reference_info = std_ref;
            (*res).image_view_binding = 0xD5 as _;
            (*slot).slot_index = 5;
            (*slot).p_next = dpb.cast();
            (*slot).p_picture_resource = res;
            (*std_pic).pic_order_cnt_val = 43;
            (*std_pic).pps_pic_parameter_set_id = 2;
            (*pic).s_type = VkStructureType(H265_PICTURE);
            (*pic).p_std_picture_info = std_pic;
            (*pic).slice_segment_count = 2;
            (*pic).p_slice_segment_offsets = offsets.cast();
            (*info).p_next = pic.cast();
            (*info).p_setup_reference_slot = slot;
            stable(st.decode(3, info))
        };
        free(std_ref);
        free(dpb);
        free(res);
        free(slot);
        free(std_pic);
        free(offsets);
        free(pic);
        free(info);
        // SAFETY: pointers into the stores.
        unsafe {
            let p = (*stable1).p_next as *const VkVideoDecodeH265PictureInfoKHR;
            assert_eq!((*p).s_type.0, H265_PICTURE);
            assert_eq!((*(*p).p_std_picture_info).pic_order_cnt_val, 43);
            assert_eq!((*(*p).p_std_picture_info).pps_pic_parameter_set_id, 2);
            assert_eq!(*(*p).p_slice_segment_offsets.add(1), 777);
            let sl = (*stable1).p_setup_reference_slot;
            assert_eq!((*(*sl).p_picture_resource).image_view_binding, 0xD5);
            let d = (*sl).p_next as *const VkVideoDecodeH265DpbSlotInfoKHR;
            assert_eq!((*d).s_type.0, H265_DPB_SLOT);
            assert_eq!((*(*d).p_std_reference_info).pic_order_cnt_val, 42);
        }
    }

    /// AV1: every pointer of the std picture info, the tile arrays, the tile
    /// offsets and sizes, and the DPB slot's std reference info outlive the
    /// raised copies; a null film grain stays null.
    #[test]
    fn av1_structures_outlive_the_call() {
        #[repr(C)]
        struct RaisedAv1 {
            std_ref: StdVideoDecodeAV1ReferenceInfo,
            dpb: VkVideoDecodeAV1DpbSlotInfoKHR,
            res: VkVideoPictureResourceInfoKHR,
            slot: VkVideoReferenceSlotInfoKHR,
            cols: [u16; 3],
            rows: [u16; 2],
            widths: [u16; 3],
            heights: [u16; 2],
            tile: StdVideoAV1TileInfo,
            quant: StdVideoAV1Quantization,
            seg: StdVideoAV1Segmentation,
            lf: StdVideoAV1LoopFilter,
            cdef: StdVideoAV1CDEF,
            lr: StdVideoAV1LoopRestoration,
            gm: StdVideoAV1GlobalMotion,
            std: StdVideoDecodeAV1PictureInfo,
            offsets: [u32; 6],
            sizes: [u32; 6],
            pic: VkVideoDecodeAV1PictureInfoKHR,
            info: VkVideoDecodeInfoKHR,
        }
        let mut st = Stores::default();
        st.session_created(0xA1, 9, 8);
        let mut begin: VkVideoBeginCodingInfoKHR = zeroed();
        begin.video_session = 0xA1 as _;
        unsafe { st.begin(4, &begin) }.unwrap();
        let r: *mut RaisedAv1 = Box::into_raw(Box::new(zeroed()));
        // SAFETY: a fresh allocation this test owns, written through `r`.
        let stable1 = unsafe {
            (*r).std_ref.order_hint = 11;
            (*r).dpb.s_type = VkStructureType(AV1_DPB_SLOT);
            (*r).dpb.p_std_reference_info = &raw const (*r).std_ref;
            (*r).res.image_view_binding = 0xE8 as _;
            (*r).slot.slot_index = 8;
            (*r).slot.p_next = (&raw const (*r).dpb).cast();
            (*r).slot.p_picture_resource = &raw const (*r).res;
            (*r).cols = [0, 10, 20];
            (*r).rows = [0, 30];
            (*r).widths = [9, 9, 4];
            (*r).heights = [29, 12];
            (*r).tile.tile_cols = 3;
            (*r).tile.tile_rows = 2;
            (*r).tile.p_mi_col_starts = (&raw const (*r).cols).cast();
            (*r).tile.p_mi_row_starts = (&raw const (*r).rows).cast();
            (*r).tile.p_width_in_sbs_minus1 = (&raw const (*r).widths).cast();
            (*r).tile.p_height_in_sbs_minus1 = (&raw const (*r).heights).cast();
            (*r).quant.base_q_idx = 99;
            (*r).cdef.cdef_bits = 2;
            (*r).std.order_hint = 12;
            (*r).std.refresh_frame_flags = 0x10;
            (*r).std.p_tile_info = &raw const (*r).tile;
            (*r).std.p_quantization = &raw const (*r).quant;
            (*r).std.p_segmentation = &raw const (*r).seg;
            (*r).std.p_loop_filter = &raw const (*r).lf;
            (*r).std.p_cdef = &raw const (*r).cdef;
            (*r).std.p_loop_restoration = &raw const (*r).lr;
            (*r).std.p_global_motion = &raw const (*r).gm;
            (*r).offsets = [0, 100, 200, 300, 400, 500];
            (*r).sizes = [100, 100, 100, 100, 100, 50];
            (*r).pic.s_type = VkStructureType(AV1_PICTURE);
            (*r).pic.p_std_picture_info = &raw const (*r).std;
            (*r).pic.reference_name_slot_indices = [-1; 7];
            (*r).pic.tile_count = 6;
            (*r).pic.p_tile_offsets = (&raw const (*r).offsets).cast();
            (*r).pic.p_tile_sizes = (&raw const (*r).sizes).cast();
            (*r).info.p_next = (&raw const (*r).pic).cast();
            (*r).info.p_setup_reference_slot = &raw const (*r).slot;
            stable(st.decode(4, &raw const (*r).info))
        };
        free(r);
        // SAFETY: pointers into the stores.
        unsafe {
            let p = (*stable1).p_next as *const VkVideoDecodeAV1PictureInfoKHR;
            assert_eq!((*p).s_type.0, AV1_PICTURE);
            assert_eq!((*p).reference_name_slot_indices, [-1; 7]);
            assert_eq!(*(*p).p_tile_offsets.add(5), 500);
            assert_eq!(*(*p).p_tile_sizes.add(5), 50);
            let s = (*p).p_std_picture_info;
            assert_eq!((*s).order_hint, 12);
            assert_eq!((*s).refresh_frame_flags, 0x10);
            let t = (*s).p_tile_info;
            assert_eq!(((*t).tile_cols, (*t).tile_rows), (3, 2));
            assert_eq!(*(*t).p_mi_col_starts.add(2), 20);
            assert_eq!(*(*t).p_mi_row_starts.add(1), 30);
            assert_eq!(*(*t).p_width_in_sbs_minus1.add(2), 4);
            assert_eq!(*(*t).p_height_in_sbs_minus1.add(1), 12);
            assert_eq!((*(*s).p_quantization).base_q_idx, 99);
            assert_eq!((*(*s).p_cdef).cdef_bits, 2);
            assert!(!(*s).p_segmentation.is_null() && !(*s).p_loop_filter.is_null());
            assert!(!(*s).p_loop_restoration.is_null() && !(*s).p_global_motion.is_null());
            assert!((*s).p_film_grain.is_null(), "no film grain was given");
            let sl = (*stable1).p_setup_reference_slot;
            assert_eq!((*sl).slot_index, 8);
            assert_eq!((*(*sl).p_picture_resource).image_view_binding, 0xE8);
            let d = (*sl).p_next as *const VkVideoDecodeAV1DpbSlotInfoKHR;
            assert_eq!((*(*d).p_std_reference_info).order_hint, 11);
        }
    }

    /// The slot guard: an index at or beyond the session's `maxDpbSlots` (the
    /// AV1 index 8 of a session with 8), or more references than it allows,
    /// is refused, in a begin and in a decode, and nothing is stored for it.
    #[test]
    fn a_slot_beyond_the_session_is_refused() {
        let mut st = Stores::default();
        st.session_created(0x8, 8, 2);
        let r8 = raised(8, 0xF8, 1);
        let mut elems = [unsafe { (*r8).slot }];
        let mut begin: VkVideoBeginCodingInfoKHR = zeroed();
        begin.video_session = 0x8 as _;
        begin.reference_slot_count = 1;
        begin.p_reference_slots = elems.as_mut_ptr();
        let e = unsafe { st.begin(5, &begin) }.unwrap_err();
        assert!(e.detail.contains("slotIndex 8") && e.detail.contains("8 DPB slots"), "{}", e.detail);
        assert_eq!(st.counts(), (0, 0));

        // An in-range begin opens the scope; a decode setting up slot 8 is refused.
        begin.reference_slot_count = 0;
        unsafe { st.begin(5, &begin) }.unwrap();
        let mut pic: VkVideoDecodeH264PictureInfoKHR = zeroed();
        pic.s_type = VkStructureType(H264_PICTURE);
        let mut info: VkVideoDecodeInfoKHR = zeroed();
        info.p_next = (&raw const pic).cast();
        info.p_setup_reference_slot = unsafe { &raw const (*r8).slot };
        assert!(matches!(unsafe { st.decode(5, &info) }, Decode::Refused(_)));

        // Three references against a limit of two.
        let refs = [raised(0, 1, 0), raised(1, 2, 0), raised(2, 3, 0)];
        let arr = refs.map(|r| unsafe { (*r).slot });
        info.p_setup_reference_slot = std::ptr::null();
        info.reference_slot_count = 3;
        info.p_reference_slots = arr.as_ptr();
        match unsafe { st.decode(5, &info) } {
            Decode::Refused(r) => assert!(r.detail.contains("3 references"), "{}", r.detail),
            other => panic!("{other:?}"),
        }
        assert_eq!(st.counts(), (0, 0));
        free(r8);
        refs.map(free);
    }

    /// The parameters calls' raise pools stay with their session until it is
    /// destroyed, and the session's stores go with it.
    #[test]
    fn parameter_pools_live_as_long_as_their_session() {
        let mut st = Stores::default();
        st.session_created(0x51, 16, 16);
        st.params_created(0x9A, 0x51);
        st.keep_params(Raise::default());
        st.params_updated(0x9A);
        st.keep_params(Raise::default());
        // A pool with no call before it is not kept.
        st.keep_params(Raise::default());
        assert_eq!(st.params_pools(), 2);
        st.params_destroyed(0x9A);
        assert_eq!(st.params_pools(), 2, "kept past the parameters object, as ffmpeg's arrays are");

        let mut begin: VkVideoBeginCodingInfoKHR = zeroed();
        begin.video_session = 0x51 as _;
        let r = raised(0, 1, 0);
        let mut elems = [unsafe { (*r).slot }];
        begin.reference_slot_count = 1;
        begin.p_reference_slots = elems.as_mut_ptr();
        unsafe { st.begin(6, &begin) }.unwrap();
        free(r);
        assert_eq!(st.counts(), (1, 0));
        st.session_destroyed(0x51);
        assert_eq!((st.counts(), st.params_pools()), ((0, 0), 0));
        assert!(st.scope.is_empty());
    }
}
