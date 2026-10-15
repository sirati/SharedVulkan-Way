//! `SV_VIDEO_LOG`: one line per video call, written **before** the driver sees
//! it (docs/video-decode.md §7.10).
//!
//! The guest's Intel driver faulted inside `vkCmdDecodeVideoKHR` replayed from a
//! command blob, after a thousand good frames (waycompose-design.md §87.12), and
//! nothing recorded what that call had been given: a recorded command has no
//! reply, the fault handler names only a ring, and the validation layer is not
//! run on the guest. This is the record. Written before the call, the last
//! `decode` line on a thread that dies inside the driver *is* the faulting call.
//!
//! What it logs is the video path's lifecycle as the driver receives it —
//! driver handles, not the application's ids:
//!
//! - the capabilities the driver answered (Intel's general `flags` were never
//!   recorded, and they decide ffmpeg's DPB layout);
//! - sessions and session parameters, with each H.264 SPS and PPS id;
//! - the images with a video usage, the views of them, the buffers a decode
//!   reads, and their memory binding;
//! - every coding scope (`begin`, `control`, `end`) and decode, with every slot,
//!   picture resource and view;
//! - the submits that carry a command buffer which recorded video.
//!
//! And it checks, on every `begin` and `decode`, the things a driver that keeps
//! its own state may dereference and RADV may not: a slot index outside the
//! session's DPB, a view or image or session already destroyed, a reference not
//! bound by the scope, a missing setup slot or codec link or standard struct,
//! and — the one the log was extended for — **the bytes at every slice offset**:
//! a start code, an H.264 slice NAL, and a `pic_parameter_set_id` that matches
//! the picture's. Intel parses slice headers on the CPU to build what its
//! hardware wants, as Mesa's ANV does (`vk_video_parse_h264_slice_header`); an
//! AMD decoder's firmware parses them on the GPU. A slice header that names a
//! PPS the session does not have is a null PPS to the first and a damaged frame
//! at worst to the second. Every finding is marked `!!` on its line.
//!
//! `SV_VIDEO_LOG=1` writes to stderr; any other non-empty value except `0` is a
//! file, opened for append, written one unbuffered `write` per line so that a
//! line is in the operating system before the call it describes is made.

use crate::server::Shared;
use ash::vk;
use ash::vk::Handle as _;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

struct Sink {
    out: Mutex<Box<dyn std::io::Write + Send>>,
    start: Instant,
}

static SINK: OnceLock<Option<Sink>> = OnceLock::new();

fn sink() -> Option<&'static Sink> {
    SINK.get_or_init(|| {
        let v = std::env::var_os("SV_VIDEO_LOG")?;
        if v.is_empty() || v == "0" {
            return None;
        }
        let out: Box<dyn std::io::Write + Send> = if v == "1" {
            Box::new(std::io::stderr())
        } else {
            match std::fs::OpenOptions::new().create(true).append(true).open(&v) {
                Ok(f) => Box::new(f),
                Err(e) => {
                    eprintln!("[vlog] cannot open {}: {e}; writing to stderr", v.to_string_lossy());
                    Box::new(std::io::stderr())
                }
            }
        };
        Some(Sink { out: Mutex::new(out), start: Instant::now() })
    })
    .as_ref()
}

/// Whether `SV_VIDEO_LOG` is on. Read once.
pub fn enabled() -> bool {
    sink().is_some()
}

fn emit(line: &str) {
    let Some(s) = sink() else { return };
    let t = s.start.elapsed().as_secs_f64();
    let th = std::thread::current();
    let name = th.name().map(str::to_string).unwrap_or_else(|| format!("{:?}", th.id()));
    let full = format!("[vlog {t:.6} {name}] {line}\n");
    let mut g = s.out.lock().unwrap_or_else(|e| e.into_inner());
    let _ = g.write_all(full.as_bytes());
    let _ = g.flush();
}

/// Write `text` as it is, if the log has a sink — for the fault report, which
/// formats its own lines.
pub fn emit_raw(text: &str) {
    let Some(s) = sink() else { return };
    // `try_lock`: this runs from an exception filter, possibly on a thread
    // that faulted while holding the sink.
    if let Ok(mut g) = s.out.try_lock() {
        let _ = g.write_all(text.as_bytes());
        let _ = g.flush();
    }
}

const VIDEO_IMAGE_USAGE: u32 = 0x400 | 0x800 | 0x1000; // DECODE_DST, DECODE_SRC, DECODE_DPB
const VIDEO_BUFFER_USAGE: u32 = 0x2000 | 0x4000; // VIDEO_DECODE_SRC, VIDEO_DECODE_DST
const DEAD_KEPT: usize = 4096;

#[derive(Clone, Copy)]
struct Session {
    max_dpb: u32,
    max_refs: u32,
    inline_params: bool,
}

#[derive(Clone, Copy)]
struct Image {
    layers: u32,
}

#[derive(Clone, Copy)]
struct View {
    image: u64,
    base: u32,
    count: u32,
}

#[derive(Clone)]
struct Scope {
    session: u64,
    slots: Vec<(i32, Option<(u64, u32)>)>,
}

/// What the log knows, keyed by **driver** handle. One device serves every
/// session, so a driver handle is unique in the process and one table is right.
#[derive(Default)]
struct State {
    sessions: HashMap<u64, Session>,
    params: HashMap<u64, u64>,
    images: HashMap<u64, Image>,
    views: HashMap<u64, View>,
    buffers: HashMap<u64, Option<(u64, u64)>>,
    scopes: HashMap<u64, Scope>,
    video_cbs: HashSet<u64>,
    dead: HashMap<u64, &'static str>,
    dead_order: VecDeque<u64>,
    /// Per session: SPS and PPS ids its parameters objects carry.
    pps_ids: HashMap<u64, HashSet<u8>>,
    decodes: u64,
    findings: u64,
}

impl State {
    fn bury(&mut self, h: u64, what: &'static str) {
        if self.dead.insert(h, what).is_none() {
            self.dead_order.push_back(h);
            if self.dead_order.len() > DEAD_KEPT
                && let Some(old) = self.dead_order.pop_front()
            {
                self.dead.remove(&old);
            }
        }
    }

    fn revive(&mut self, h: u64) {
        self.dead.remove(&h);
    }

    /// A view as a decode names it, checked: alive, of a live image, and the
    /// layer inside the view.
    fn view(&self, view: u64, layer: u32, what: &str, findings: &mut Vec<String>) -> String {
        if view == 0 {
            findings.push(format!("{what} has a null imageViewBinding"));
            return "v0".into();
        }
        match self.views.get(&view) {
            Some(v) => {
                if layer >= v.count {
                    findings.push(format!(
                        "{what} baseArrayLayer {layer} outside view 0x{view:x}'s {} layer(s)",
                        v.count
                    ));
                }
                if !self.images.contains_key(&v.image) {
                    let why = self.dead.get(&v.image).copied().unwrap_or("never seen");
                    findings.push(format!("{what} view 0x{view:x} is of image 0x{:x}: {why}", v.image));
                }
                format!("v{view:x}@{}+{layer}", v.base)
            }
            None => {
                match self.dead.get(&view) {
                    Some(why) => findings.push(format!("{what} view 0x{view:x} {why}")),
                    None => findings.push(format!("{what} view 0x{view:x} was never created as a video view")),
                }
                format!("v{view:x}?")
            }
        }
    }
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE
        .get_or_init(|| Mutex::new(State::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// # Safety
///
/// `addr` is zero or the address of a live, aligned `T`.
unsafe fn at<'a, T>(addr: u64) -> Option<&'a T> {
    // SAFETY: the caller's contract.
    unsafe { (addr as usize as *const T).as_ref() }
}

/// # Safety
///
/// `p` is null or points at `n` live, aligned `T`s.
unsafe fn many<'a, T>(p: *const T, n: u32) -> &'a [T] {
    if p.is_null() || n == 0 {
        return &[];
    }
    // SAFETY: the caller's contract.
    unsafe { std::slice::from_raw_parts(p, n as usize) }
}

/// The first link of `s_type` in a `pNext` chain.
///
/// # Safety
///
/// `p` is null or the head of a well-formed chain of live structures.
unsafe fn find_link(mut p: *const std::ffi::c_void, s_type: vk::StructureType) -> *const std::ffi::c_void {
    while !p.is_null() {
        // SAFETY: every Vulkan structure begins with `sType` and `pNext`.
        let b = unsafe { &*(p as *const vk::BaseInStructure<'static>) };
        if b.s_type == s_type {
            return p;
        }
        p = b.p_next as *const _;
    }
    std::ptr::null()
}

/// The entry point the generated forwarder's `GpuHost::observe` calls.
///
/// # Safety
///
/// `args` are the C arguments of `command` as the generated dispatch passes
/// them to the driver, every pointer live for the duration of this call (see
/// `GpuHost::observe`). An output is read only when `after` is true and
/// `result` is `VK_SUCCESS`.
pub unsafe fn observe(shared: &Shared, command: &'static str, after: bool, args: &[u64], result: i32) {
    if !enabled() {
        return;
    }
    let a = |i: usize| args.get(i).copied().unwrap_or(0);
    // SAFETY (every arm): the function's contract, and the signature of the
    // named command, which fixes what each argument is.
    unsafe {
        match (command, after) {
            ("vkGetPhysicalDeviceVideoCapabilitiesKHR", true) => caps(a(1), a(2), result),
            ("vkCreateVideoSessionKHR", true) => create_session(a(1), a(3), result),
            ("vkDestroyVideoSessionKHR", false) => destroy(a(1), "session", "destroyed (vkDestroyVideoSessionKHR)"),
            ("vkCreateVideoSessionParametersKHR", true) => create_params(a(1), a(3), result),
            ("vkUpdateVideoSessionParametersKHR", false) => update_params(a(1), a(2)),
            ("vkDestroyVideoSessionParametersKHR", false) => {
                destroy(a(1), "params", "destroyed (vkDestroyVideoSessionParametersKHR)")
            }
            ("vkCreateImage", true) => create_image(a(1), a(3), result),
            ("vkDestroyImage", false) => destroy(a(1), "image", "destroyed (vkDestroyImage)"),
            ("vkCreateImageView", true) => create_view(a(1), a(3), result),
            ("vkDestroyImageView", false) => destroy(a(1), "view", "destroyed (vkDestroyImageView)"),
            ("vkDestroyBuffer", false) => destroy(a(1), "buffer", "destroyed (vkDestroyBuffer)"),
            ("vkBindBufferMemory", false) => bind_buffer(a(1), a(2), a(3)),
            ("vkCmdBeginVideoCodingKHR", false) => begin(a(0), a(1)),
            ("vkCmdControlVideoCodingKHR", false) => control(a(0), a(1)),
            ("vkCmdDecodeVideoKHR", false) => decode(shared, a(0), a(1)),
            ("vkCmdDecodeVideoKHR", true) => {
                // Only reached if the driver returned: the call before it is
                // the one to read if it did not.
            }
            ("vkCmdEndVideoCodingKHR", false) => end(a(0)),
            ("vkQueueSubmit", false) => submit(a(0), a(1), a(2), false),
            ("vkQueueSubmit2", false) => submit(a(0), a(1), a(2), true),
            _ => {}
        }
    }
}

/// `vkCreateBuffer` is hand-written on this side, so it says so itself.
pub fn buffer_created(buffer: u64, usage: u32, size: u64, result: i32) {
    if !enabled() || result != 0 || usage & VIDEO_BUFFER_USAGE == 0 {
        return;
    }
    let mut s = state();
    s.buffers.insert(buffer, None);
    s.revive(buffer);
    drop(s);
    emit(&format!("buffer b{buffer:x} created: {size} B, usage 0x{usage:x}"));
}

unsafe fn caps(profile: u64, out: u64, result: i32) {
    // SAFETY: `observe`'s contract.
    let (Some(p), Some(c)) = (unsafe { at::<vk::VideoProfileInfoKHR<'static>>(profile) }, unsafe {
        at::<vk::VideoCapabilitiesKHR<'static>>(out)
    }) else {
        return;
    };
    if result != 0 {
        emit(&format!("caps codec 0x{:x}: VkResult {result}", p.video_codec_operation.as_raw()));
        return;
    }
    // SAFETY: an output chain the driver has just filled.
    let d = unsafe { find_link(c.p_next, vk::StructureType::VIDEO_DECODE_CAPABILITIES_KHR) };
    let dflags = if d.is_null() {
        "-".to_string()
    } else {
        // SAFETY: the link's `sType` says what it is.
        format!("0x{:x}", unsafe { &*(d as *const vk::VideoDecodeCapabilitiesKHR<'static>) }.flags.as_raw())
    };
    emit(&format!(
        "caps codec 0x{:x}: flags 0x{:x} (SEPARATE_REFERENCE_IMAGES={}) decode flags {dflags} dpb slots {} \
         active refs {} bitstream align {}/{} coded {}x{}..{}x{}",
        p.video_codec_operation.as_raw(),
        c.flags.as_raw(),
        c.flags.contains(vk::VideoCapabilityFlagsKHR::SEPARATE_REFERENCE_IMAGES) as u8,
        c.max_dpb_slots,
        c.max_active_reference_pictures,
        c.min_bitstream_buffer_offset_alignment,
        c.min_bitstream_buffer_size_alignment,
        c.min_coded_extent.width,
        c.min_coded_extent.height,
        c.max_coded_extent.width,
        c.max_coded_extent.height,
    ));
}

unsafe fn create_session(ci: u64, out: u64, result: i32) {
    // SAFETY: `observe`'s contract.
    let Some(ci) = (unsafe { at::<vk::VideoSessionCreateInfoKHR<'static>>(ci) }) else { return };
    // SAFETY: an output the driver wrote.
    let h = if result == 0 { unsafe { at::<u64>(out) }.copied().unwrap_or(0) } else { 0 };
    // SAFETY: `pVideoProfile` is a required pointer of a live create info.
    let codec = unsafe { at::<vk::VideoProfileInfoKHR<'static>>(ci.p_video_profile as u64) }
        .map_or(0, |p| p.video_codec_operation.as_raw());
    let inline_params = ci.flags.as_raw() & 0x10 != 0; // INLINE_SESSION_PARAMETERS
    emit(&format!(
        "session s{h:x} created (VkResult {result}): codec 0x{codec:x} family {} flags 0x{:x} max {}x{} \
         dpb slots {} active refs {} formats {}/{}",
        ci.queue_family_index,
        ci.flags.as_raw(),
        ci.max_coded_extent.width,
        ci.max_coded_extent.height,
        ci.max_dpb_slots,
        ci.max_active_reference_pictures,
        ci.picture_format.as_raw(),
        ci.reference_picture_format.as_raw(),
    ));
    if h != 0 {
        let mut s = state();
        s.sessions.insert(
            h,
            Session {
                max_dpb: ci.max_dpb_slots,
                max_refs: ci.max_active_reference_pictures,
                inline_params,
            },
        );
        s.revive(h);
    }
}

/// The SPS and PPS ids an H.264 add-info carries, and a line describing them.
///
/// # Safety
///
/// `add` is null or a live `VkVideoDecodeH264SessionParametersAddInfoKHR`.
unsafe fn h264_add(add: *const vk::VideoDecodeH264SessionParametersAddInfoKHR<'static>) -> (String, Vec<u8>, Vec<String>) {
    let mut findings = Vec::new();
    // SAFETY: the caller's contract.
    let Some(add) = (unsafe { add.as_ref() }) else {
        return ("no add info".into(), Vec::new(), findings);
    };
    let mut text = String::new();
    // SAFETY: counted arrays of a live add-info.
    let sps = unsafe { many(add.p_std_sp_ss, add.std_sps_count) };
    let pps = unsafe { many(add.p_std_pp_ss, add.std_pps_count) };
    if add.std_sps_count > 0 && sps.is_empty() {
        findings.push(format!("stdSPSCount {} with a null pStdSPSs", add.std_sps_count));
    }
    if add.std_pps_count > 0 && pps.is_empty() {
        findings.push(format!("stdPPSCount {} with a null pStdPPSs", add.std_pps_count));
    }
    for s in sps {
        let _ = write!(
            text,
            " sps{}(profile {} level {} {}x{}mb refs {} scaling {} vui {})",
            s.seq_parameter_set_id,
            s.profile_idc,
            s.level_idc,
            s.pic_width_in_mbs_minus1 + 1,
            s.pic_height_in_map_units_minus1 + 1,
            s.max_num_ref_frames,
            !s.pScalingLists.is_null() as u8,
            !s.pSequenceParameterSetVui.is_null() as u8,
        );
    }
    let mut ids = Vec::new();
    for p in pps {
        let _ = write!(text, " pps{}(sps {} scaling {})", p.pic_parameter_set_id, p.seq_parameter_set_id, !p.pScalingLists.is_null() as u8);
        ids.push(p.pic_parameter_set_id);
    }
    (text, ids, findings)
}

unsafe fn create_params(ci: u64, out: u64, result: i32) {
    // SAFETY: `observe`'s contract.
    let Some(ci) = (unsafe { at::<vk::VideoSessionParametersCreateInfoKHR<'static>>(ci) }) else { return };
    // SAFETY: an output the driver wrote.
    let h = if result == 0 { unsafe { at::<u64>(out) }.copied().unwrap_or(0) } else { 0 };
    let session = ci.video_session.as_raw();
    let template = ci.video_session_parameters_template.as_raw();
    // SAFETY: the create info's own chain.
    let l = unsafe { find_link(ci.p_next, vk::StructureType::VIDEO_DECODE_H264_SESSION_PARAMETERS_CREATE_INFO_KHR) };
    let mut findings = Vec::new();
    let (text, ids) = if l.is_null() {
        ("(not H.264)".to_string(), Vec::new())
    } else {
        // SAFETY: the link's `sType` says what it is.
        let h264 = unsafe { &*(l as *const vk::VideoDecodeH264SessionParametersCreateInfoKHR<'static>) };
        // SAFETY: a member pointer of a live link.
        let (t, ids, f) = unsafe { h264_add(h264.p_parameters_add_info) };
        findings.extend(f);
        (format!("max sps {} pps {}:{t}", h264.max_std_sps_count, h264.max_std_pps_count), ids)
    };
    let mut s = state();
    if !s.sessions.contains_key(&session) {
        let why = s.dead.get(&session).copied().unwrap_or("never seen");
        findings.push(format!("its session s{session:x} is {why}"));
    }
    if template != 0 && !s.params.contains_key(&template) {
        findings.push(format!("its template p{template:x} is not a live parameters object"));
    }
    if h != 0 {
        s.params.insert(h, session);
        s.revive(h);
        let mut set: HashSet<u8> = ids.into_iter().collect();
        if template != 0 {
            set.extend(s.pps_ids.get(&template).cloned().unwrap_or_default());
        }
        s.pps_ids.insert(h, set);
    }
    s.findings += findings.len() as u64;
    drop(s);
    emit(&format!(
        "params p{h:x} created (VkResult {result}) for s{session:x} template p{template:x} {text}{}",
        flag(&findings)
    ));
}

unsafe fn update_params(params: u64, info: u64) {
    // SAFETY: `observe`'s contract.
    let Some(u) = (unsafe { at::<vk::VideoSessionParametersUpdateInfoKHR<'static>>(info) }) else { return };
    // SAFETY: the update info's own chain.
    let l = unsafe { find_link(u.p_next, vk::StructureType::VIDEO_DECODE_H264_SESSION_PARAMETERS_ADD_INFO_KHR) };
    // SAFETY: the link's `sType` says what it is.
    let (text, ids, findings) = unsafe { h264_add(l as *const _) };
    let mut s = state();
    if let Some(set) = s.pps_ids.get_mut(&params) {
        set.extend(ids);
    }
    drop(s);
    emit(&format!("params p{params:x} update #{}:{text}{}", u.update_sequence_count, flag(&findings)));
}

unsafe fn destroy(h: u64, kind: &'static str, why: &'static str) {
    if h == 0 {
        return;
    }
    let mut s = state();
    let known = match kind {
        "session" => s.sessions.remove(&h).is_some(),
        "params" => {
            s.pps_ids.remove(&h);
            s.params.remove(&h).is_some()
        }
        "image" => s.images.remove(&h).is_some(),
        "view" => s.views.remove(&h).is_some(),
        "buffer" => s.buffers.remove(&h).is_some(),
        _ => false,
    };
    if !known {
        return;
    }
    s.bury(h, why);
    // A scope that is still open on a session being destroyed is worth saying.
    let open: Vec<u64> = s.scopes.iter().filter(|(_, sc)| sc.session == h).map(|(cb, _)| *cb).collect();
    drop(s);
    let note = if open.is_empty() {
        String::new()
    } else {
        format!(" !! while command buffer(s) {open:x?} have a coding scope open on it")
    };
    emit(&format!("{kind} {h:x} {why}{note}"));
}

unsafe fn create_image(ci: u64, out: u64, result: i32) {
    // SAFETY: `observe`'s contract.
    let Some(ci) = (unsafe { at::<vk::ImageCreateInfo<'static>>(ci) }) else { return };
    let usage = ci.usage.as_raw();
    if result != 0 || usage & VIDEO_IMAGE_USAGE == 0 {
        return;
    }
    // SAFETY: an output the driver wrote.
    let h = unsafe { at::<u64>(out) }.copied().unwrap_or(0);
    let mut s = state();
    s.images.insert(h, Image { layers: ci.array_layers });
    s.revive(h);
    drop(s);
    emit(&format!(
        "image i{h:x} created: {}x{} layers {} format {} usage 0x{usage:x} flags 0x{:x}",
        ci.extent.width,
        ci.extent.height,
        ci.array_layers,
        ci.format.as_raw(),
        ci.flags.as_raw()
    ));
}

unsafe fn create_view(ci: u64, out: u64, result: i32) {
    // SAFETY: `observe`'s contract.
    let Some(ci) = (unsafe { at::<vk::ImageViewCreateInfo<'static>>(ci) }) else { return };
    let image = ci.image.as_raw();
    let mut s = state();
    let Some(img) = s.images.get(&image).copied() else { return };
    if result != 0 {
        drop(s);
        emit(&format!("view of i{image:x}: VkResult {result}"));
        return;
    }
    // SAFETY: an output the driver wrote.
    let h = unsafe { at::<u64>(out) }.copied().unwrap_or(0);
    let r = ci.subresource_range;
    let count = if r.layer_count == vk::REMAINING_ARRAY_LAYERS {
        img.layers.saturating_sub(r.base_array_layer)
    } else {
        r.layer_count
    };
    s.views.insert(h, View { image, base: r.base_array_layer, count });
    // A driver that keeps a DPB slot's picture by its view object, rather
    // than by the view each call names, would be holding this address from
    // before (docs/video-decode.md §7.10, candidate 1). Said, not flagged:
    // reuse is legal and common.
    let reused = if s.dead.contains_key(&h) { " (the address of a destroyed view, reused)" } else { "" };
    s.revive(h);
    drop(s);
    emit(&format!(
        "view v{h:x} created: image i{image:x} layers {}+{count} type {} format {}{reused}",
        r.base_array_layer,
        ci.view_type.as_raw(),
        ci.format.as_raw()
    ));
}

unsafe fn bind_buffer(buffer: u64, memory: u64, offset: u64) {
    let mut s = state();
    let Some(slot) = s.buffers.get_mut(&buffer) else { return };
    *slot = Some((memory, offset));
    drop(s);
    emit(&format!("buffer b{buffer:x} bound to m{memory:x}+{offset}"));
}

/// A picture resource, checked against the session and the scope.
///
/// # Safety
///
/// `slot` is a live `VkVideoReferenceSlotInfoKHR`.
unsafe fn slot_text(
    st: &State,
    slot: &vk::VideoReferenceSlotInfoKHR<'static>,
    session: Option<&Session>,
    what: &str,
    findings: &mut Vec<String>,
) -> (String, Option<(u64, u32)>) {
    if let Some(sess) = session
        && (slot.slot_index < -1 || slot.slot_index >= sess.max_dpb as i32)
    {
        findings.push(format!("{what} slotIndex {} outside the session's {} DPB slots", slot.slot_index, sess.max_dpb));
    }
    // SAFETY: a member pointer of a live slot.
    match unsafe { slot.p_picture_resource.as_ref() } {
        None => (format!("{}:-", slot.slot_index), None),
        Some(r) => {
            let view = r.image_view_binding.as_raw();
            let v = st.view(view, r.base_array_layer, &format!("{what} slot {}", slot.slot_index), findings);
            (format!("{}:{v} {}x{}", slot.slot_index, r.coded_extent.width, r.coded_extent.height), Some((view, r.base_array_layer)))
        }
    }
}

unsafe fn begin(cb: u64, info: u64) {
    // SAFETY: `observe`'s contract.
    let Some(b) = (unsafe { at::<vk::VideoBeginCodingInfoKHR<'static>>(info) }) else { return };
    let session = b.video_session.as_raw();
    let params = b.video_session_parameters.as_raw();
    let mut s = state();
    s.video_cbs.insert(cb);
    let mut findings = Vec::new();
    let sess = s.sessions.get(&session).copied();
    if sess.is_none() {
        let why = s.dead.get(&session).copied().unwrap_or("never seen");
        findings.push(format!("session s{session:x} is {why}"));
    }
    if params != 0 {
        match s.params.get(&params) {
            Some(&owner) if owner != session => {
                findings.push(format!("params p{params:x} belong to session s{owner:x}"))
            }
            Some(_) => {}
            None => {
                let why = s.dead.get(&params).copied().unwrap_or("never seen");
                findings.push(format!("params p{params:x} are {why}"));
            }
        }
    }
    // SAFETY: a counted array of a live begin info.
    let slots = unsafe { many(b.p_reference_slots, b.reference_slot_count) };
    if b.reference_slot_count > 0 && slots.is_empty() {
        findings.push(format!("referenceSlotCount {} with a null pReferenceSlots", b.reference_slot_count));
    }
    let mut text = String::new();
    let mut kept = Vec::new();
    let mut seen = HashSet::new();
    for sl in slots {
        // SAFETY: an element of a live array.
        let (t, res) = unsafe { slot_text(&s, sl, sess.as_ref(), "begin", &mut findings) };
        if sl.slot_index >= 0 && !seen.insert(sl.slot_index) {
            findings.push(format!("begin binds slot {} twice", sl.slot_index));
        }
        let _ = write!(text, " {t}");
        kept.push((sl.slot_index, res));
    }
    if let Some(sess) = sess
        && b.reference_slot_count > sess.max_refs + 1
    {
        findings.push(format!(
            "{} reference slots against maxActiveReferencePictures {}",
            b.reference_slot_count, sess.max_refs
        ));
    }
    if s.scopes.insert(cb, Scope { session, slots: kept }).is_some() {
        findings.push("a coding scope was already open on this command buffer".into());
    }
    s.findings += findings.len() as u64;
    drop(s);
    emit(&format!("begin cb{cb:x} s{session:x} p{params:x} slots [{} ]{}", text, flag(&findings)));
}

unsafe fn control(cb: u64, info: u64) {
    // SAFETY: `observe`'s contract.
    let Some(c) = (unsafe { at::<vk::VideoCodingControlInfoKHR<'static>>(info) }) else { return };
    let open = state().scopes.contains_key(&cb);
    emit(&format!(
        "control cb{cb:x} flags 0x{:x}{}",
        c.flags.as_raw(),
        if open { "" } else { " !! outside a coding scope" }
    ));
}

unsafe fn end(cb: u64) {
    let open = state().scopes.remove(&cb).is_some();
    emit(&format!("end cb{cb:x}{}", if open { "" } else { " !! no coding scope was open" }));
}

unsafe fn submit(queue: u64, count: u64, submits: u64, two: bool) {
    let mut cbs = Vec::new();
    if two {
        // SAFETY: `observe`'s contract: `count` live `VkSubmitInfo2`s.
        for si in unsafe { many(submits as usize as *const vk::SubmitInfo2<'static>, count as u32) } {
            // SAFETY: a counted array of a live submit.
            for c in unsafe { many(si.p_command_buffer_infos, si.command_buffer_info_count) } {
                cbs.push(c.command_buffer.as_raw());
            }
        }
    } else {
        // SAFETY: as above, for `VkSubmitInfo`.
        for si in unsafe { many(submits as usize as *const vk::SubmitInfo<'static>, count as u32) } {
            // SAFETY: a counted array of a live submit.
            for c in unsafe { many(si.p_command_buffers, si.command_buffer_count) } {
                cbs.push(c.as_raw());
            }
        }
    }
    let s = state();
    if !cbs.iter().any(|c| s.video_cbs.contains(c)) {
        return;
    }
    let open: Vec<u64> = cbs.iter().copied().filter(|c| s.scopes.contains_key(c)).collect();
    drop(s);
    let note = if open.is_empty() { String::new() } else { format!(" !! submitted with a coding scope open: {open:x?}") };
    emit(&format!("submit q{queue:x} cbs {cbs:x?}{note}"));
}

/// Read up to `n` bytes of the arena behind `(memory, offset)`.
fn arena_bytes(shared: &Shared, memory: u64, offset: u64, n: usize) -> Option<Vec<u8>> {
    let at = shared.objects().arena_offset_of(memory, offset)?;
    let len = shared.arena.region.len() as u64;
    if at >= len {
        return None;
    }
    let n = n.min((len - at) as usize);
    let mut buf = vec![0u8; n];
    shared.arena.region.read_bytes(at, &mut buf).ok()?;
    Some(buf)
}

/// An H.264 slice header's start: what a CPU-side parser reads first.
#[derive(Debug, PartialEq, Eq)]
pub struct SliceStart {
    pub nal_type: u8,
    pub first_mb: u32,
    pub slice_type: u32,
    pub pps_id: u32,
}

/// Parse `00 00 01`, the NAL header and three `ue(v)`s, removing emulation
/// prevention bytes. `None` if the bytes are not a start code and a NAL.
pub fn parse_slice_start(b: &[u8]) -> Option<SliceStart> {
    let rest = b.strip_prefix(&[0, 0, 1])?;
    let (&nal, rbsp) = rest.split_first()?;
    if nal & 0x80 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(rbsp.len());
    let mut zeros = 0;
    for &x in rbsp {
        if zeros >= 2 && x == 3 {
            zeros = 0;
            continue;
        }
        zeros = if x == 0 { zeros + 1 } else { 0 };
        bytes.push(x);
    }
    let mut pos = 0usize;
    let mut bit = || -> Option<u32> {
        let byte = *bytes.get(pos / 8)?;
        let v = (byte >> (7 - pos % 8)) & 1;
        pos += 1;
        Some(v as u32)
    };
    let mut ue = || -> Option<u32> {
        let mut lz = 0;
        while bit()? == 0 {
            lz += 1;
            if lz > 31 {
                return None;
            }
        }
        let mut v = 0u32;
        for _ in 0..lz {
            v = (v << 1) | bit()?;
        }
        Some((1u32 << lz) - 1 + v)
    };
    let first_mb = ue()?;
    let slice_type = ue()?;
    let pps_id = ue()?;
    Some(SliceStart { nal_type: nal & 0x1f, first_mb, slice_type, pps_id })
}

unsafe fn decode(shared: &Shared, cb: u64, info: u64) {
    // SAFETY: `observe`'s contract.
    let Some(d) = (unsafe { at::<vk::VideoDecodeInfoKHR<'static>>(info) }) else { return };
    let mut s = state();
    s.decodes += 1;
    let n = s.decodes;
    s.video_cbs.insert(cb);
    let mut findings = Vec::new();
    let scope = s.scopes.get(&cb).cloned();
    if scope.is_none() {
        findings.push("no coding scope is open on this command buffer".into());
    }
    let sess = scope.as_ref().and_then(|sc| s.sessions.get(&sc.session).copied());

    // Source.
    let src = d.src_buffer.as_raw();
    let binding = match s.buffers.get(&src) {
        Some(b) => *b,
        None => {
            let why = s.dead.get(&src).copied().unwrap_or("never created with a video usage");
            findings.push(format!("srcBuffer b{src:x} {why}"));
            None
        }
    };

    // Destination.
    let dst = &d.dst_picture_resource;
    let dst_text = s.view(dst.image_view_binding.as_raw(), dst.base_array_layer, "dst", &mut findings);

    // Setup slot.
    // SAFETY: a member pointer of a live decode info.
    let setup = match unsafe { d.p_setup_reference_slot.as_ref() } {
        None => "-".to_string(),
        Some(sl) => {
            // SAFETY: a live slot.
            let (t, _) = unsafe { slot_text(&s, sl, sess.as_ref(), "setup", &mut findings) };
            if sl.slot_index < 0 {
                findings.push(format!("setup slotIndex {}", sl.slot_index));
            }
            if sl.p_picture_resource.is_null() {
                findings.push("setup slot has no picture resource".into());
            }
            // SAFETY: the slot's own chain.
            let l = unsafe { find_link(sl.p_next, vk::StructureType::VIDEO_DECODE_H264_DPB_SLOT_INFO_KHR) };
            // SAFETY: the link's `sType` says what it is.
            if !l.is_null() && unsafe { &*(l as *const vk::VideoDecodeH264DpbSlotInfoKHR<'static>) }.p_std_reference_info.is_null() {
                findings.push("setup slot's H.264 DPB info has a null pStdReferenceInfo".into());
            }
            t
        }
    };

    // References.
    // SAFETY: a counted array of a live decode info.
    let refs = unsafe { many(d.p_reference_slots, d.reference_slot_count) };
    if d.reference_slot_count > 0 && refs.is_empty() {
        findings.push(format!("referenceSlotCount {} with a null pReferenceSlots", d.reference_slot_count));
    }
    let mut refs_text = String::new();
    let mut seen = HashSet::new();
    for r in refs {
        // SAFETY: an element of a live array.
        let (t, res) = unsafe { slot_text(&s, r, sess.as_ref(), "ref", &mut findings) };
        let _ = write!(refs_text, " {t}");
        if r.slot_index < 0 {
            findings.push(format!("reference slotIndex {}", r.slot_index));
        }
        if !seen.insert(r.slot_index) {
            findings.push(format!("reference slot {} named twice", r.slot_index));
        }
        if r.p_picture_resource.is_null() {
            findings.push(format!("reference slot {} has no picture resource", r.slot_index));
        }
        if let Some(sc) = &scope {
            match sc.slots.iter().find(|(i, _)| *i == r.slot_index) {
                None => findings.push(format!("reference slot {} is not bound by the scope's begin", r.slot_index)),
                Some((_, bound)) if *bound != res => findings.push(format!(
                    "reference slot {}'s resource {:x?} is not the one begin bound, {:x?}",
                    r.slot_index, res, bound
                )),
                Some(_) => {}
            }
        }
        // SAFETY: the slot's own chain.
        let l = unsafe { find_link(r.p_next, vk::StructureType::VIDEO_DECODE_H264_DPB_SLOT_INFO_KHR) };
        if !l.is_null() {
            // SAFETY: the link's `sType` says what it is.
            let info = unsafe { &*(l as *const vk::VideoDecodeH264DpbSlotInfoKHR<'static>) };
            // SAFETY: a member pointer of a live link.
            match unsafe { info.p_std_reference_info.as_ref() } {
                None => findings.push(format!("reference slot {}: null pStdReferenceInfo", r.slot_index)),
                Some(ri) => {
                    let _ = write!(refs_text, "(fn{} poc{}/{})", ri.FrameNum, ri.PicOrderCnt[0], ri.PicOrderCnt[1]);
                }
            }
        }
    }

    // The codec link, and the bytes at every slice offset.
    // SAFETY: the decode info's own chain.
    let l = unsafe { find_link(d.p_next, vk::StructureType::VIDEO_DECODE_H264_PICTURE_INFO_KHR) };
    let mut codec = String::new();
    if !l.is_null() {
        // SAFETY: the link's `sType` says what it is.
        let h = unsafe { &*(l as *const vk::VideoDecodeH264PictureInfoKHR<'static>) };
        // SAFETY: a member pointer of a live link.
        let pic = unsafe { h.p_std_picture_info.as_ref() };
        // SAFETY: a counted array of a live link.
        let offsets = unsafe { many(h.p_slice_offsets, h.slice_count) };
        if h.slice_count > 0 && offsets.is_empty() {
            findings.push(format!("sliceCount {} with a null pSliceOffsets", h.slice_count));
        }
        match pic {
            None => findings.push("H.264 picture info has a null pStdPictureInfo".into()),
            Some(p) => {
                let _ = write!(
                    codec,
                    " h264 sps{} pps{} frame_num {} idr {} poc {}/{} idr_flag {} ref {} intra {}",
                    p.seq_parameter_set_id,
                    p.pic_parameter_set_id,
                    p.frame_num,
                    p.idr_pic_id,
                    p.PicOrderCnt[0],
                    p.PicOrderCnt[1],
                    p.flags.IdrPicFlag(),
                    p.flags.is_reference(),
                    p.flags.is_intra(),
                );
                if let Some(sc) = &scope
                    && let Some(sess) = s.sessions.get(&sc.session)
                    && !sess.inline_params
                {
                    // Which parameters objects this session has, and whether
                    // any of them carries the picture's PPS.
                    let any = s
                        .params
                        .iter()
                        .filter(|(_, owner)| **owner == sc.session)
                        .any(|(h, _)| s.pps_ids.get(h).is_some_and(|ids| ids.contains(&p.pic_parameter_set_id)));
                    if !any {
                        findings.push(format!("no live parameters object of the session carries pps {}", p.pic_parameter_set_id));
                    }
                }
            }
        }
        let _ = write!(codec, " slices");
        for &off in offsets {
            let at = d.src_buffer_offset + off as u64;
            if off as u64 >= d.src_buffer_range {
                findings.push(format!("slice offset {off} beyond srcBufferRange {}", d.src_buffer_range));
            }
            let bytes = binding.and_then(|(m, mo)| arena_bytes(shared, m, mo + at, 24));
            match bytes {
                None => {
                    let _ = write!(codec, " @{off}:?");
                }
                Some(b) => match parse_slice_start(&b) {
                    None => {
                        let _ = write!(codec, " @{off}:{:02x?}", &b[..b.len().min(8)]);
                        findings.push(format!("slice at +{off} does not start with a start code and a NAL: {:02x?}", &b[..b.len().min(8)]));
                    }
                    Some(ss) => {
                        let _ = write!(codec, " @{off}:nal{} mb{} t{} pps{}", ss.nal_type, ss.first_mb, ss.slice_type, ss.pps_id);
                        if ss.nal_type != 1 && ss.nal_type != 5 {
                            findings.push(format!("slice at +{off} is NAL type {}, not a slice", ss.nal_type));
                        }
                        if let Some(p) = pic {
                            if u32::from(p.pic_parameter_set_id) != ss.pps_id {
                                findings.push(format!(
                                    "slice at +{off} names pps {} but the picture says pps {}",
                                    ss.pps_id, p.pic_parameter_set_id
                                ));
                            }
                            if (ss.nal_type == 5) != (p.flags.IdrPicFlag() != 0) {
                                findings.push(format!("slice at +{off} is NAL {} but IdrPicFlag is {}", ss.nal_type, p.flags.IdrPicFlag()));
                            }
                        }
                    }
                },
            }
        }
    } else if d.p_next.is_null() {
        findings.push("no codec picture info in pNext".into());
    }
    if let Some(sess) = &sess
        && refs.len() as u32 > sess.max_refs
    {
        findings.push(format!("{} references against maxActiveReferencePictures {}", refs.len(), sess.max_refs));
    }
    s.findings += findings.len() as u64;
    drop(s);
    let line = format!(
        "decode #{n} cb{cb:x} src b{src:x}{}+{}/{} dst {dst_text} setup {setup} refs [{refs_text} ]{codec}{}",
        binding.map_or(String::new(), |(m, o)| format!("(m{m:x}+{o})")),
        d.src_buffer_offset,
        d.src_buffer_range,
        flag(&findings)
    );
    if let Ok(mut last) = LAST_DECODE.lock() {
        last.clone_from(&line);
    }
    emit(&line);
}

/// The last decode line, kept apart from [`State`] so that the fault handler
/// can read it even if a thread died holding the state lock.
static LAST_DECODE: Mutex<String> = Mutex::new(String::new());

/// What the log knew when the process faulted, for `crate::fault`'s report.
///
/// Called from an unhandled-exception filter, on whichever thread faulted:
/// it only `try_lock`s, so a lock held by the dying thread costs a line saying
/// so rather than a deadlock, and it allocates nothing it does not already
/// own the means to allocate (the filter itself formats strings already).
pub fn fault_summary() -> String {
    if !enabled() {
        return "[vlog] SV_VIDEO_LOG was off: no record of the video calls\n".into();
    }
    let mut out = String::new();
    match LAST_DECODE.try_lock() {
        Ok(l) => {
            let _ = writeln!(out, "[vlog] FAULT: last decode handed to a driver: {}", *l);
        }
        Err(_) => out.push_str("[vlog] FAULT: the last-decode line was locked\n"),
    }
    match STATE.get().map(|m| m.try_lock()) {
        Some(Ok(s)) => {
            let _ = writeln!(
                out,
                "[vlog] FAULT: {} decodes, {} findings; live: {} session(s), {} params, {} video image(s), \
                 {} view(s), {} bitstream buffer(s); {} coding scope(s) open on {:x?}; {} command buffer(s) \
                 have recorded video",
                s.decodes,
                s.findings,
                s.sessions.len(),
                s.params.len(),
                s.images.len(),
                s.views.len(),
                s.buffers.len(),
                s.scopes.len(),
                s.scopes.keys().collect::<Vec<_>>(),
                s.video_cbs.len(),
            );
        }
        Some(Err(_)) => out.push_str("[vlog] FAULT: the log's state was locked by a thread that stopped\n"),
        None => out.push_str("[vlog] FAULT: no video call was ever observed\n"),
    }
    out
}

fn flag(findings: &[String]) -> String {
    if findings.is_empty() {
        String::new()
    } else {
        format!(" !! {}", findings.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slice_start_is_read_through_emulation_prevention() {
        // 00 00 01 | 65 (nal_ref_idc 3, IDR) | ue(0)=1 ue(7)=0001000 ue(0)=1 ...
        // bits: 1 0001000 1 -> 1000 1000 1xxx xxxx
        let b = [0, 0, 1, 0x65, 0b1000_1000, 0b1000_0000];
        assert_eq!(
            parse_slice_start(&b),
            Some(SliceStart { nal_type: 5, first_mb: 0, slice_type: 7, pps_id: 0 })
        );
        // A non-IDR slice naming pps 2: ue(0)=1, ue(5)=00110, ue(2)=011
        // bits: 1 00110 011 -> 1001 1001 1xxx
        let b = [0, 0, 1, 0x41, 0b1001_1001, 0b1000_0000];
        assert_eq!(
            parse_slice_start(&b),
            Some(SliceStart { nal_type: 1, first_mb: 0, slice_type: 5, pps_id: 2 })
        );
        // Emulation prevention: 00 00 03 inside the RBSP is dropped. first_mb
        // = ue with 16 leading zeros then 1 and 16 bits.
        let b = [0, 0, 1, 0x41, 0, 0, 3, 0x80, 0, 0x80, 0xff, 0xff];
        let s = parse_slice_start(&b).unwrap();
        assert_eq!(s.first_mb, 1 << 16);
        // Not a start code.
        assert_eq!(parse_slice_start(&[0, 0, 0, 1, 0x65, 0x88]), None);
        // Stale bytes after a slice: a forbidden_zero_bit.
        assert_eq!(parse_slice_start(&[0, 0, 1, 0xff, 0x88]), None);
    }

    #[test]
    fn a_view_the_log_never_saw_or_saw_destroyed_is_a_finding() {
        let mut st = State::default();
        st.images.insert(0x10, Image { layers: 1 });
        st.views.insert(0x20, View { image: 0x10, base: 0, count: 1 });
        let mut f = Vec::new();
        assert!(st.view(0x20, 0, "dst", &mut f).starts_with("v20"));
        assert!(f.is_empty());
        st.view(0x20, 1, "dst", &mut f);
        assert!(f[0].contains("outside view"), "{f:?}");
        f.clear();
        st.views.remove(&0x20);
        st.bury(0x20, "destroyed (vkDestroyImageView)");
        st.view(0x20, 0, "ref slot 3", &mut f);
        assert!(f[0].contains("destroyed"), "{f:?}");
        f.clear();
        st.view(0x30, 0, "ref slot 3", &mut f);
        assert!(f[0].contains("never created"), "{f:?}");
    }

    /// The decode check reads raw structures; Miri runs this.
    #[test]
    fn a_decode_info_is_read_without_touching_anything_it_does_not_own() {
        let pic = vk::native::StdVideoDecodeH264PictureInfo {
            flags: unsafe { std::mem::zeroed() },
            seq_parameter_set_id: 0,
            pic_parameter_set_id: 0,
            reserved1: 0,
            reserved2: 0,
            frame_num: 3,
            idr_pic_id: 0,
            PicOrderCnt: [6, 6],
        };
        let offsets = [0u32];
        let h264 = vk::VideoDecodeH264PictureInfoKHR {
            p_std_picture_info: &pic,
            slice_count: 1,
            p_slice_offsets: offsets.as_ptr(),
            ..Default::default()
        };
        let res = vk::VideoPictureResourceInfoKHR {
            image_view_binding: vk::ImageView::from_raw(0x20),
            ..Default::default()
        };
        let setup = vk::VideoReferenceSlotInfoKHR { slot_index: 20, p_picture_resource: &res, ..Default::default() };
        let d = vk::VideoDecodeInfoKHR {
            p_next: (&raw const h264).cast(),
            p_setup_reference_slot: &setup,
            ..Default::default()
        };
        let mut st = State::default();
        st.sessions.insert(1, Session { max_dpb: 16, max_refs: 16, inline_params: true });
        st.images.insert(0x10, Image { layers: 1 });
        st.views.insert(0x20, View { image: 0x10, base: 0, count: 1 });
        let mut f = Vec::new();
        // SAFETY: every pointer above is live for the call.
        let (t, res_seen) = unsafe { slot_text(&st, &setup, st.sessions.get(&1), "setup", &mut f) };
        assert!(t.starts_with("20:v20"), "{t}");
        assert_eq!(res_seen, Some((0x20, 0)));
        assert!(f.iter().any(|x| x.contains("outside the session's 16 DPB slots")), "{f:?}");
        // SAFETY: as above.
        let l = unsafe { find_link(d.p_next, vk::StructureType::VIDEO_DECODE_H264_PICTURE_INFO_KHR) };
        assert_eq!(l, (&raw const h264).cast());
        // SAFETY: as above.
        assert_eq!(unsafe { many(h264.p_slice_offsets, h264.slice_count) }, &[0]);
    }
}
