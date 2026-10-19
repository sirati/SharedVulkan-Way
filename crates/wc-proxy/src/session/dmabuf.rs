//! `zwp_linux_dmabuf_v1`, answered here (docs/waycompose-design.md §104).
//!
//! Only with `--dmabuf-node PATH`. Off, nothing in this file runs and the
//! proxy is byte for byte what it was. On, `PATH` is stat()ed when a client's
//! registry is filled (right after the compositor's `wl_shm` global), and the
//! client is offered the global only if `PATH` is a character device at that
//! moment: on the guest's host the node is a CUSE device root makes after the
//! proxy started, and it may go and come back. One line per client says which.
//!
//! # What the client is told, and what the compositor is told
//!
//! The client sees a `zwp_linux_dmabuf_v1` global, version 4, whose default
//! feedback names `PATH`'s `st_rdev` as the main device and offers
//! `ARGB8888`/`XRGB8888` with the linear modifier. A "dma-buf" it hands over is
//! an `sv_drm::token` — a sealed memfd naming a shared allocation — and the
//! compositor never hears the word: it is told
//! `wl_shm.create_pool(Section{name})` (or `SharedFile{path}` for a name that
//! is a unix path), `wl_shm_pool.create_buffer`, `wl_shm_pool.destroy`, all of
//! which it has understood since stage 3. Its `wl_shm` mapping *is* the shared
//! allocation, so a commit of such a buffer copies nothing here.
//!
//! # Ids, which are the whole difficulty
//!
//! A client's object ids are a sequence, and the compositor's
//! `wayland-backend` accepts a client `new_id` only if it is at most one past
//! the highest slot it has (`rs/map.rs`, `insert_in_at`). Every object this
//! file answers locally would leave a hole upstream, and the next object the
//! client creates would be refused (`Invalid new_id`), so every one of them is
//! *filled* upstream, at the same id, by something harmless:
//!
//! * `zwp_linux_dmabuf_v1` (bound on the private global) -> a `wl_shm` bound
//!   on the same registry, version 1. It is also the `wl_shm` the pools are
//!   created on, so this proxy never needs an id of its own. Its `format`
//!   events are dropped. `wl_shm` v1 has no destructor, so the client's
//!   `destroy` is absorbed and **never acknowledged**: the client keeps the id
//!   as a zombie forever, which costs one id per bind (KWin binds once) and is
//!   the only way the id cannot be reused while the compositor still has it.
//! * `zwp_linux_dmabuf_feedback_v1`, `zwp_linux_buffer_params_v1` ->
//!   `wl_display.sync`. The compositor creates the callback, sends `done` and
//!   destroys it at once, so the slot is filled and then vacant; the `done`
//!   and the `delete_id` are dropped by count ([`Dmabuf::swallow_done`],
//!   [`Dmabuf::swallow_delete`]). Counting is exact because the compositor
//!   answers in order: everything it says about the placeholder precedes
//!   anything it says about a later object with the same id.
//! * The params id is then used once more, as the upstream id of the
//!   `wl_shm_pool` a buffer is cut from — the slot is vacant by then, for the
//!   reason above — and that pool is destroyed in the same breath. Its
//!   `delete_id` is dropped as well.
//! * A buffer from `create_immed` is the client's own `wl_buffer` id upstream.
//!   One that failed is filled with a `sync` like the rest and its `destroy`
//!   is absorbed and acknowledged here.

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _, OwnedFd};

use wc_handle::Descriptor;

use super::{op, Session};
use crate::spec::ArgKind;
use crate::wire::{self, Span};

/// The registry name of the global this proxy adds. The compositor's names
/// count up from 1 (smithay's `GlobalId`s); this is near the top of the range
/// so the two cannot meet, and [`Session::dmabuf_event`] checks anyway.
pub const GLOBAL_NAME: u32 = 0xfffe_d4b0;
/// The version advertised. KWin needs 4 (feedback); 5 adds nothing this proxy
/// would do differently but is not asked for.
pub const VERSION: u32 = 4;

/// DRM fourccs.
pub const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
pub const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
/// `DRM_FORMAT_MOD_LINEAR`.
pub const MOD_LINEAR: u64 = 0;

/// What the format table offers, in table order, with the `wl_shm` format the
/// compositor is told for each. Only the two the compositor's `wl_shm` path
/// reads (`wc-compositor`'s `ShmSource::with_pixels` refuses every other
/// format), so `ABGR8888`/`XBGR8888` are not offered.
pub const FORMATS: [(u32, u32); 2] = [(DRM_FORMAT_ARGB8888, 0), (DRM_FORMAT_XRGB8888, 1)];

pub(super) mod ops {
    // zwp_linux_dmabuf_v1 requests
    pub const DMABUF_DESTROY: u16 = 0;
    pub const DMABUF_CREATE_PARAMS: u16 = 1;
    pub const DMABUF_GET_DEFAULT_FEEDBACK: u16 = 2;
    pub const DMABUF_GET_SURFACE_FEEDBACK: u16 = 3;
    // zwp_linux_dmabuf_v1 events (before version 4)
    pub const DMABUF_FORMAT: u16 = 0;
    pub const DMABUF_MODIFIER: u16 = 1;
    // zwp_linux_buffer_params_v1 requests
    pub const PARAMS_DESTROY: u16 = 0;
    pub const PARAMS_ADD: u16 = 1;
    pub const PARAMS_CREATE: u16 = 2;
    pub const PARAMS_CREATE_IMMED: u16 = 3;
    // zwp_linux_buffer_params_v1 events
    pub const PARAMS_FAILED: u16 = 1;
    // zwp_linux_dmabuf_feedback_v1 request
    pub const FEEDBACK_DESTROY: u16 = 0;
    // zwp_linux_dmabuf_feedback_v1 events
    pub const FB_DONE: u16 = 0;
    pub const FB_FORMAT_TABLE: u16 = 1;
    pub const FB_MAIN_DEVICE: u16 = 2;
    pub const FB_TRANCHE_DONE: u16 = 3;
    pub const FB_TRANCHE_TARGET_DEVICE: u16 = 4;
    pub const FB_TRANCHE_FORMATS: u16 = 5;
    pub const FB_TRANCHE_FLAGS: u16 = 6;
    // wl_registry
    pub const REGISTRY_BIND: u16 = 0;
    pub const REGISTRY_GLOBAL: u16 = 0;
    // wl_display
    pub const DISPLAY_SYNC: u16 = 0;
}

/// One `zwp_linux_buffer_params_v1`.
#[derive(Default)]
struct Params {
    /// The `zwp_linux_dmabuf_v1` it came from, which upstream is the `wl_shm`
    /// its pool is created on.
    shm: u32,
    /// Plane 0, once added.
    plane: Option<Plane>,
    /// Why this params object can no longer make a buffer, if it cannot: a
    /// second plane, a bad fd. Reported at `create`/`create_immed`.
    broken: Option<String>,
    used: bool,
}

struct Plane {
    token: sv_drm::token::Token,
    offset: u32,
    stride: u32,
    modifier: u64,
}

/// Everything about this connection's dma-bufs.
pub(super) struct Dmabuf {
    /// `--dmabuf-node`. Stat()ed whenever it matters rather than once: on the
    /// guest's host the node is a CUSE device root creates after this proxy
    /// started, and it can go away and come back between sessions.
    node: std::path::PathBuf,
    /// `st_rdev` of the node when this client was last offered the global,
    /// the feedback's main and target device. `None`: not offered.
    dev: Option<u64>,
    /// What was last said about the offer, so that a client with several
    /// registries gets one line, and another only if the answer changed.
    said: Option<Option<u64>>,
    /// The global has been announced to this client on some registry, so a
    /// bind of it is ours to answer even if the node has gone since.
    offered: bool,
    /// The compositor's `wl_shm` global, learned from its registry. The
    /// private global is announced right after it, so it is always known by
    /// the time a client can bind.
    shm_global: Option<u32>,
    /// Ids that are a `zwp_linux_dmabuf_v1` here and a `wl_shm` upstream.
    /// Every event on them is the compositor's `wl_shm.format` and is dropped.
    shm_shadow: HashSet<u32>,
    /// Placeholder `wl_callback.done`s still to come, per id.
    swallow_done: HashMap<u32, u32>,
    /// `wl_display.delete_id`s still to come for objects this file made
    /// upstream, per id.
    swallow_delete: HashMap<u32, u32>,
    params: HashMap<u32, Params>,
    /// `wl_buffer`s that are a shared allocation upstream.
    pub(super) buffers: HashSet<u32>,
    /// `wl_buffer`s whose `create_immed` failed: a placeholder upstream.
    dead_buffers: HashSet<u32>,
}

impl Dmabuf {
    pub(super) fn new(node: std::path::PathBuf) -> Dmabuf {
        Dmabuf {
            node,
            dev: None,
            said: None,
            offered: false,
            shm_global: None,
            shm_shadow: HashSet::new(),
            swallow_done: HashMap::new(),
            swallow_delete: HashMap::new(),
            params: HashMap::new(),
            buffers: HashSet::new(),
            dead_buffers: HashSet::new(),
        }
    }
}

/// `path`'s `st_rdev`, if it is a character device right now.
pub fn render_node(path: &std::path::Path) -> io::Result<u64> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    let m = std::fs::metadata(path)?;
    if !m.file_type().is_char_device() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a character device"));
    }
    Ok(m.rdev())
}

/// glibc's `gnu_dev_major`/`gnu_dev_minor`, for the log line.
pub fn dev_major(dev: u64) -> u64 {
    ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)
}

pub fn dev_minor(dev: u64) -> u64 {
    (dev & 0xff) | ((dev >> 12) & !0xff)
}

fn bump(m: &mut HashMap<u32, u32>, id: u32) {
    *m.entry(id).or_insert(0) += 1;
}

/// Take one off `m[id]`; true if there was one.
fn take(m: &mut HashMap<u32, u32>, id: u32) -> bool {
    match m.get_mut(&id) {
        Some(n) if *n > 0 => {
            *n -= 1;
            if *n == 0 {
                m.remove(&id);
            }
            true
        }
        _ => false,
    }
}

/// The format table: one 16-byte entry per format, `{u32 format, u32 pad,
/// u64 modifier}`, in a sealed memfd.
pub fn format_table() -> io::Result<(OwnedFd, Vec<u8>)> {
    let mut table = Vec::with_capacity(FORMATS.len() * 16);
    for (fourcc, _) in FORMATS {
        table.extend_from_slice(&fourcc.to_ne_bytes());
        table.extend_from_slice(&0u32.to_ne_bytes());
        table.extend_from_slice(&MOD_LINEAR.to_ne_bytes());
    }
    // SAFETY: a NUL-terminated literal and valid flags.
    let raw = unsafe {
        libc::memfd_create(
            c"wc-proxy-dmabuf-formats".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: freshly returned, owned from here.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut at = 0;
    while at < table.len() {
        // SAFETY: a live fd and a slice of `table`.
        let n = unsafe {
            libc::pwrite(fd.as_raw_fd(), table[at..].as_ptr().cast(), table.len() - at, at as i64)
        };
        if n <= 0 {
            return Err(io::Error::last_os_error());
        }
        at += n as usize;
    }
    // Sealed so that a client may map it however it likes and nobody can
    // change it under it; the spec asks clients to map it MAP_PRIVATE.
    // SAFETY: a live fd and valid seal flags.
    unsafe {
        libc::fcntl(
            fd.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL,
        )
    };
    Ok((fd, table))
}

/// Why a buffer cannot be made from these parameters, or what to tell the
/// compositor if it can: `(descriptor, size, offset, stride, wl_shm format)`.
fn validate(
    p: &Params,
    width: i32,
    height: i32,
    format: u32,
    flags: u32,
) -> Result<(Descriptor, u64, u32, u32, u32), String> {
    if p.used {
        return Err("already_used: the params object already made a buffer".into());
    }
    if let Some(why) = &p.broken {
        return Err(why.clone());
    }
    let plane = p.plane.as_ref().ok_or("incomplete: no plane 0")?;
    if plane.modifier != MOD_LINEAR {
        return Err(format!("invalid_format: modifier {:#x} is not LINEAR", plane.modifier));
    }
    let shm_format = FORMATS
        .iter()
        .find(|(f, _)| *f == format)
        .map(|(_, s)| *s)
        .ok_or_else(|| format!("invalid_format: format {format:#010x} is not in the table"))?;
    if flags != 0 {
        return Err(format!("flags {flags:#x}: y-invert/interlaced are not supported"));
    }
    if width <= 0 || height <= 0 {
        return Err(format!("invalid_dimensions: {width}x{height}"));
    }
    let (w, h) = (width as u64, height as u64);
    let (offset, stride) = (u64::from(plane.offset), u64::from(plane.stride));
    if stride < w * 4 {
        return Err(format!("out_of_bounds: stride {stride} < width {width} * 4"));
    }
    let end = offset + stride * (h - 1) + w * 4;
    let size = plane.token.size;
    if end > size {
        return Err(format!(
            "out_of_bounds: offset {offset} + stride {stride} * {} + {} = {end} > {size} bytes",
            h - 1,
            w * 4
        ));
    }
    // `wl_shm` sizes and offsets are `int`s.
    if size > i32::MAX as u64 || offset > i32::MAX as u64 || stride > i32::MAX as u64 {
        return Err(format!("out_of_bounds: {size} bytes do not fit a wl_shm pool"));
    }
    let name = plane.token.name.clone();
    let d = if name.starts_with('/') {
        Descriptor::SharedFile { path: name, size }
    } else {
        Descriptor::Section { name, size }
    };
    Ok((d, size, plane.offset, plane.stride, shm_format))
}

impl Session {
    /// Offer `zwp_linux_dmabuf_v1` to this client if `node` is a character
    /// device when its registry is filled, with that `st_rdev` as the main
    /// device. See this module.
    pub fn set_dmabuf_node(&mut self, node: Option<std::path::PathBuf>) {
        self.dmabuf = node.map(Dmabuf::new);
    }

    /// `wl_display.sync(id)`, filling `id`'s slot upstream; its `done` and
    /// `delete_id` are dropped on the way back.
    fn placeholder(&mut self, id: u32, out: &mut Vec<u8>) {
        let d = self.dmabuf.as_mut().expect("dmabuf on");
        bump(&mut d.swallow_done, id);
        bump(&mut d.swallow_delete, id);
        out.extend_from_slice(&wire::message(1, ops::DISPLAY_SYNC, &id.to_ne_bytes()));
    }

    /// A request, before it is forwarded. `Ok(Some(bytes))`: consumed, and
    /// `bytes` go upstream instead. `Ok(None)`: carry on as before.
    pub(super) fn dmabuf_request(
        &mut self,
        interface: &str,
        opcode: u16,
        object: u32,
        body: &[u8],
        spans: &[Span],
        fds: &mut std::collections::VecDeque<OwnedFd>,
    ) -> Result<Option<Vec<u8>>, super::Error> {
        let Some(d) = self.dmabuf.as_mut() else { return Ok(None) };
        let word = |i: usize| spans.get(i).and_then(|s| wire::word_at(body, s)).unwrap_or(0);
        let mut out = Vec::new();
        match (interface, opcode) {
            ("wl_registry", ops::REGISTRY_BIND) if word(0) == GLOBAL_NAME && d.offered => {
                let Some((iface, version, id)) = spans
                    .get(1)
                    .filter(|s| s.kind == ArgKind::NewIdUntyped)
                    .and_then(|s| wire::untyped_new_id(body, s))
                else {
                    return Err(super::Error::Walk("wl_registry.bind: no new_id".into()));
                };
                let Some(shm_name) = d.shm_global else {
                    // Cannot happen: the global is announced only after
                    // `wl_shm`'s. Said rather than assumed.
                    return Err(super::Error::Walk(
                        "zwp_linux_dmabuf_v1 bound before the compositor announced wl_shm".into(),
                    ));
                };
                if iface != "zwp_linux_dmabuf_v1" {
                    return Err(super::Error::Walk(format!(
                        "global {GLOBAL_NAME:#x} is zwp_linux_dmabuf_v1, not {iface}"
                    )));
                }
                d.shm_shadow.insert(id);
                let mut b = Vec::new();
                b.extend_from_slice(&shm_name.to_ne_bytes());
                wire::push_string(&mut b, "wl_shm");
                b.extend_from_slice(&1u32.to_ne_bytes());
                b.extend_from_slice(&id.to_ne_bytes());
                out.extend_from_slice(&wire::message(object, ops::REGISTRY_BIND, &b));
                // Before version 4 the formats are events on the global.
                if version < 4 {
                    for (fourcc, _) in FORMATS {
                        if version >= 3 {
                            let w = [fourcc, 0, 0];
                            let b: Vec<u8> = w.iter().flat_map(|x| x.to_ne_bytes()).collect();
                            self.to_client
                                .extend_from_slice(&wire::message(id, ops::DMABUF_MODIFIER, &b));
                        } else {
                            self.to_client.extend_from_slice(&wire::message(
                                id,
                                ops::DMABUF_FORMAT,
                                &fourcc.to_ne_bytes(),
                            ));
                        }
                    }
                }
                self.note(format!("zwp_linux_dmabuf_v1@{id} v{version}: a wl_shm upstream"));
                Ok(Some(out))
            }
            ("zwp_linux_dmabuf_v1", ops::DMABUF_DESTROY) => {
                // Absorbed and never acknowledged: see the module header.
                Ok(Some(out))
            }
            ("zwp_linux_dmabuf_v1", ops::DMABUF_CREATE_PARAMS) => {
                let id = word(0);
                d.params.insert(id, Params { shm: object, ..Params::default() });
                self.placeholder(id, &mut out);
                Ok(Some(out))
            }
            ("zwp_linux_dmabuf_v1", ops::DMABUF_GET_DEFAULT_FEEDBACK | ops::DMABUF_GET_SURFACE_FEEDBACK) => {
                let id = word(0);
                self.placeholder(id, &mut out);
                self.send_feedback(id)?;
                Ok(Some(out))
            }
            ("zwp_linux_dmabuf_feedback_v1", ops::FEEDBACK_DESTROY) => {
                self.fabricate_delete_id(object);
                Ok(Some(out))
            }
            ("zwp_linux_buffer_params_v1", ops::PARAMS_DESTROY) => {
                d.params.remove(&object);
                self.fabricate_delete_id(object);
                Ok(Some(out))
            }
            ("zwp_linux_buffer_params_v1", ops::PARAMS_ADD) => {
                let fd = fds.pop_front().ok_or_else(|| super::Error::MissingFd {
                    interface: interface.into(),
                    message: "add".into(),
                })?;
                self.stats.fds_received += 1;
                // Out-of-band fd: the body is plane_idx, offset, stride,
                // modifier_hi, modifier_lo.
                let (plane_idx, offset, stride) = (word(1), word(2), word(3));
                let modifier = (u64::from(word(4)) << 32) | u64::from(word(5));
                let read = sv_drm::token::read(fd.as_fd());
                drop(fd); // the token is read; the fd is not kept
                let d = self.dmabuf.as_mut().expect("dmabuf on");
                let Some(p) = d.params.get_mut(&object) else { return Ok(Some(out)) };
                match (plane_idx, read) {
                    (0, Ok(token)) if p.plane.is_none() => {
                        p.plane = Some(Plane { token, offset, stride, modifier });
                    }
                    (0, Ok(_)) => p.broken = Some("plane_set: plane 0 added twice".into()),
                    (0, Err(e)) => p.broken = Some(format!("invalid_wl_buffer: {e}")),
                    (n, _) => p.broken = Some(format!("plane_idx: plane {n}, only one plane is supported")),
                }
                Ok(Some(out))
            }
            ("zwp_linux_buffer_params_v1", ops::PARAMS_CREATE) => {
                // `create` answers with a `wl_buffer` the *server* allocates,
                // which this proxy would have to translate in every message
                // naming it; `create_immed` (what KWin and Mesa use) has the
                // client's id on both sides. So `create` always fails, and
                // says why.
                let (w, h, format, flags) = (word(0) as i32, word(1) as i32, word(2), word(3));
                let p = d.params.entry(object).or_default();
                let why = match validate(p, w, h, format, flags) {
                    Ok(_) => "create is not supported here; use create_immed".to_owned(),
                    Err(e) => e,
                };
                p.used = true;
                self.stats.dmabuf_failed += 1;
                self.note(format!("zwp_linux_buffer_params_v1@{object}.create failed: {why}"));
                self.to_client
                    .extend_from_slice(&wire::message(object, ops::PARAMS_FAILED, &[]));
                Ok(Some(out))
            }
            ("zwp_linux_buffer_params_v1", ops::PARAMS_CREATE_IMMED) => {
                let buffer = word(0);
                let (w, h, format, flags) = (word(1) as i32, word(2) as i32, word(3), word(4));
                let p = d.params.entry(object).or_default();
                let verdict = validate(p, w, h, format, flags);
                p.used = true;
                let shm = p.shm;
                match verdict {
                    Ok((desc, size, offset, stride, shm_format)) => {
                        let described = format!("{desc:?}");
                        let mut b = Vec::new();
                        b.extend_from_slice(&object.to_ne_bytes());
                        wire::push_bytes(&mut b, &desc.encode());
                        b.extend_from_slice(&(size as u32).to_ne_bytes());
                        out.extend_from_slice(&wire::message(shm, op::SHM_CREATE_POOL, &b));
                        let words = [buffer, offset, w as u32, h as u32, stride, shm_format];
                        let b: Vec<u8> = words.iter().flat_map(|x| x.to_ne_bytes()).collect();
                        out.extend_from_slice(&wire::message(object, op::POOL_CREATE_BUFFER, &b));
                        out.extend_from_slice(&wire::message(object, op::POOL_DESTROY, &[]));
                        bump(&mut d.swallow_delete, object);
                        d.buffers.insert(buffer);
                        self.foreign_buffers.insert(buffer);
                        self.stats.dmabuf_buffers += 1;
                        self.note(format!(
                            "dma-buf wl_buffer@{buffer}: {w}x{h} stride {stride} offset {offset} \
                             -> {described}, zero copy"
                        ));
                    }
                    Err(why) => {
                        // A failed buffer rather than a protocol error: this
                        // proxy cannot end its client's connection with a
                        // `wl_display.error` cleanly, and the spec allows
                        // either (see `create_immed` in linux-dmabuf-v1.xml).
                        self.foreign_buffers.remove(&buffer);
                        d.dead_buffers.insert(buffer);
                        self.stats.dmabuf_failed += 1;
                        self.placeholder(buffer, &mut out);
                        self.note(format!(
                            "zwp_linux_buffer_params_v1@{object}.create_immed failed: {why}"
                        ));
                        self.to_client
                            .extend_from_slice(&wire::message(object, ops::PARAMS_FAILED, &[]));
                    }
                }
                Ok(Some(out))
            }
            ("wl_buffer", op::BUFFER_DESTROY) if d.dead_buffers.remove(&object) => {
                self.fabricate_delete_id(object);
                Ok(Some(out))
            }
            ("wl_buffer", op::BUFFER_DESTROY) => {
                d.buffers.remove(&object);
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// The default (and every surface's) feedback, in the order the protocol
    /// gives: table, main device, one tranche, done.
    fn send_feedback(&mut self, id: u32) -> Result<(), super::Error> {
        // Asked again: the node may have been recreated (with another minor)
        // since the global was offered. If it is gone, the device this
        // client was offered is still the truest answer there is.
        let d = self.dmabuf.as_mut().expect("dmabuf on");
        let dev = match render_node(&d.node) {
            Ok(dev) => {
                d.dev = Some(dev);
                dev
            }
            Err(_) => d.dev.unwrap_or(0),
        };
        let (fd, table) = format_table()?;
        self.clip.to_client_fds.push(fd);
        let tc = &mut self.to_client;
        tc.extend_from_slice(&wire::message(
            id,
            ops::FB_FORMAT_TABLE,
            &(table.len() as u32).to_ne_bytes(),
        ));
        let mut device = Vec::new();
        wire::push_bytes(&mut device, &dev.to_ne_bytes());
        tc.extend_from_slice(&wire::message(id, ops::FB_MAIN_DEVICE, &device));
        tc.extend_from_slice(&wire::message(id, ops::FB_TRANCHE_TARGET_DEVICE, &device));
        tc.extend_from_slice(&wire::message(id, ops::FB_TRANCHE_FLAGS, &0u32.to_ne_bytes()));
        let indices: Vec<u8> =
            (0..FORMATS.len() as u16).flat_map(|i| i.to_ne_bytes()).collect();
        let mut b = Vec::new();
        wire::push_bytes(&mut b, &indices);
        tc.extend_from_slice(&wire::message(id, ops::FB_TRANCHE_FORMATS, &b));
        tc.extend_from_slice(&wire::message(id, ops::FB_TRANCHE_DONE, &[]));
        tc.extend_from_slice(&wire::message(id, ops::FB_DONE, &[]));
        Ok(())
    }

    /// An event, before anything else looks at it. `Some(bytes)`: this is what
    /// the client gets instead (often nothing). `None`: carry on.
    pub(super) fn dmabuf_event(&mut self, object: u32, opcode: u16, body: &[u8]) -> Option<Vec<u8>> {
        let d = self.dmabuf.as_mut()?;
        if object == 1 && opcode == op::DISPLAY_DELETE_ID && body.len() >= 4 {
            let id = u32::from_ne_bytes(body[..4].try_into().expect("four bytes"));
            if take(&mut d.swallow_delete, id) {
                return Some(Vec::new());
            }
            return None;
        }
        if take(&mut d.swallow_done, object) || d.shm_shadow.contains(&object) {
            return Some(Vec::new());
        }
        None
    }

    /// After an event has been translated: watch the registry, and announce
    /// the private global right after the compositor's `wl_shm`.
    pub(super) fn dmabuf_registry(&mut self, interface: &str, object: u32, opcode: u16, body: &[u8], spans: &[Span], out: &mut Vec<u8>) {
        let Some(d) = self.dmabuf.as_mut() else { return };
        if interface != "wl_registry" || opcode != ops::REGISTRY_GLOBAL {
            return;
        }
        let name = spans.first().and_then(|s| wire::word_at(body, s)).unwrap_or(0);
        let iface = spans.get(1).and_then(|s| wire::string_at(body, s)).unwrap_or_default();
        if name == GLOBAL_NAME {
            self.notes.push(format!(
                "the compositor announced global {name:#x} ({iface}), the name this proxy uses \
                 for zwp_linux_dmabuf_v1; hidden"
            ));
            out.clear();
            return;
        }
        match iface.as_str() {
            "wl_shm" => {
                d.shm_global = Some(name);
                // Asked now, for this client, and not at startup: see `node`.
                let now = render_node(&d.node);
                let dev = now.as_ref().ok().copied();
                d.dev = dev;
                if d.said != Some(dev) {
                    d.said = Some(dev);
                    let line = match &now {
                        Ok(dev) => format!(
                            "offered zwp_linux_dmabuf_v1 v{VERSION}, main device {}:{} ({})",
                            dev_major(*dev),
                            dev_minor(*dev),
                            d.node.display()
                        ),
                        Err(e) => format!(
                            "not offered zwp_linux_dmabuf_v1: {}: {e}",
                            d.node.display()
                        ),
                    };
                    self.notes.push(line);
                }
                if dev.is_none() {
                    return;
                }
                d.offered = true;
                let mut b = Vec::new();
                b.extend_from_slice(&GLOBAL_NAME.to_ne_bytes());
                wire::push_string(&mut b, "zwp_linux_dmabuf_v1");
                b.extend_from_slice(&VERSION.to_ne_bytes());
                out.extend_from_slice(&wire::message(object, ops::REGISTRY_GLOBAL, &b));
            }
            // The proxy's own global replaces any the compositor might offer.
            "zwp_linux_dmabuf_v1" => out.clear(),
            _ => {}
        }
    }

    /// Whether `buffer` is a shared allocation, for the commit path.
    pub(super) fn is_dmabuf_buffer(&self, buffer: u32) -> bool {
        self.dmabuf.as_ref().is_some_and(|d| d.buffers.contains(&buffer))
    }

    /// Whether `buffer` is a failed `create_immed`'s placeholder, which the
    /// compositor has as no `wl_buffer` at all: an attach of it is held back.
    pub(super) fn is_placeholder_buffer(&self, buffer: u32) -> bool {
        self.dmabuf.as_ref().is_some_and(|d| d.dead_buffers.contains(&buffer))
    }
}
