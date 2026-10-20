//! The dma-buf path, app side (waycompose-design.md §104).
//!
//! Turned on only by `SV_DRM_NODE` naming a character device — the userspace
//! render node `sv-drm serve` provides, or any char device in a test. Without
//! it nothing here changes a single call: every hook below starts with
//! [`enabled`].
//!
//! With it, Mesa's GBM and Zink can allocate, export and import LINEAR images
//! whose memory is a guest-side named shared section ("zero copy" for KWin's
//! nested compositor):
//!
//! - the six extensions of [`EXTENSIONS`] are advertised, and answered here;
//! - the one DRM format modifier is `DRM_FORMAT_MOD_LINEAR`, and a modifier
//!   image is a LINEAR image to the GPU side;
//! - an exportable allocation is `svAllocShared`, an import `svImportShared`,
//!   and the "dma-buf" is a sealed memfd token carrying the section's name
//!   (`sv_drm::token`);
//! - a `SYNC_FD` export returns -1 (already signalled) at once and numbers the
//!   work for the GPU side's fence record, which the token's sync page names
//!   to whoever reads the buffer next (§104.11, [`Fences`]); an import drops
//!   the wait it would add;
//! - `FOREIGN`/`EXTERNAL` queue families become `IGNORED` before the GPU side
//!   sees them.

use sv_proto as p;

/// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT`.
pub const DMA_BUF: u32 = 0x200;
/// `VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT`: what the GPU side
/// is told a dma-buf image is.
pub const HOST_ALLOCATION: u32 = 0x80;
/// `VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT`.
pub const SYNC_FD: u32 = 0x10;
pub const TILING_LINEAR: i32 = 1;
/// `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT`.
pub const TILING_DRM_FORMAT_MODIFIER: i32 = 1_000_158_000;
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
pub const QUEUE_FAMILY_IGNORED: u32 = !0;
pub const QUEUE_FAMILY_EXTERNAL: u32 = !1;
pub const QUEUE_FAMILY_FOREIGN: u32 = !2;
/// `VK_IMAGE_ASPECT_COLOR_BIT`.
pub const ASPECT_COLOR: u32 = 0x1;
/// `VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT`.
pub const ASPECT_MEMORY_PLANE_0: u32 = 0x80;
/// `VK_IMAGE_ASPECT_PLANE_0_BIT`; planes 1 and 2 are the next two bits, as
/// the memory planes are after `ASPECT_MEMORY_PLANE_0`.
pub const ASPECT_PLANE_0: u32 = 0x10;

/// How many planes a format has (§112): 2 for the `_2PLANE_` formats, 3 for
/// the `_3PLANE_` ones, 1 for everything else. A LINEAR modifier image has as
/// many memory planes as its format has planes.
pub fn plane_count(format: i32) -> u32 {
    match format {
        // G8/G10X6/G12X4/G16 x {3P420, 2P420, 3P422, 2P422, 3P444}
        1_000_156_002 | 1_000_156_004 | 1_000_156_006 => 3,
        1_000_156_003 | 1_000_156_005 => 2,
        1_000_156_012 | 1_000_156_014 | 1_000_156_016 => 3,
        1_000_156_013 | 1_000_156_015 => 2,
        1_000_156_022 | 1_000_156_024 | 1_000_156_026 => 3,
        1_000_156_023 | 1_000_156_025 => 2,
        1_000_156_029 | 1_000_156_031 | 1_000_156_033 => 3,
        1_000_156_030 | 1_000_156_032 => 2,
        // the four 2PLANE_444 formats
        1_000_330_000..=1_000_330_003 => 2,
        _ => 1,
    }
}
/// `VK_PIPELINE_STAGE_ALL_COMMANDS_BIT`.
pub const STAGE_ALL_COMMANDS: u32 = 0x10000;

pub const VK_ERROR_MEMORY_MAP_FAILED: i32 = -5;
pub const VK_ERROR_FORMAT_NOT_SUPPORTED: i32 = -11;
pub const VK_ERROR_OUT_OF_DEVICE_MEMORY: i32 = -2;
pub const VK_ERROR_INVALID_EXTERNAL_HANDLE: i32 = -1_000_072_003;
pub const VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT: i32 = -1_000_158_000;

/// `VK_EXTERNAL_MEMORY_FEATURE_*`: dedicated only, exportable, importable.
const MEMORY_FEATURES: u32 = 0x1 | 0x2 | 0x4;
const IMPORTABLE: u32 = 0x4;

/// The extensions this path implements, with the spec versions advertised.
/// Added to whatever the GPU side forwards; none of them reaches its driver.
pub const EXTENSIONS: &[(&str, u32)] = &[
    ("VK_KHR_external_memory_fd", 1),
    ("VK_EXT_external_memory_dma_buf", 1),
    ("VK_EXT_image_drm_format_modifier", 2),
    ("VK_EXT_queue_family_foreign", 1),
    ("VK_EXT_physical_device_drm", 1),
    ("VK_KHR_external_semaphore_fd", 1),
];

/// `SV_DRM_NODE`'s device number, `(major, minor)`, when it names a character
/// device. Read once.
pub fn node() -> Option<(u32, u32)> {
    static NODE: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
    *NODE.get_or_init(|| {
        let path = std::env::var_os("SV_DRM_NODE")?;
        let n = stat_char_device(std::path::Path::new(&path));
        match n {
            Some((maj, min)) => eprintln!(
                "sv-icd: dma-buf path on, render node {} ({maj}:{min})",
                std::path::Path::new(&path).display()
            ),
            None => eprintln!(
                "sv-icd: SV_DRM_NODE={} is not a character device; the dma-buf path stays off",
                std::path::Path::new(&path).display()
            ),
        }
        n
    })
}

#[cfg(target_os = "linux")]
fn stat_char_device(path: &std::path::Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let m = std::fs::metadata(path).ok()?;
    if !m.file_type().is_char_device() {
        return None;
    }
    let rdev = m.rdev();
    Some((libc::major(rdev), libc::minor(rdev)))
}

#[cfg(not(target_os = "linux"))]
fn stat_char_device(_path: &std::path::Path) -> Option<(u32, u32)> {
    None
}

#[cfg(test)]
thread_local! { static TEST_ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(test)]
pub(crate) fn test_enabled_scope<T>(f: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset { fn drop(&mut self) { TEST_ENABLED.set(self.0); } }
    let _reset = Reset(TEST_ENABLED.replace(true));
    f()
}
/// Is the dma-buf path on?
pub fn enabled() -> bool {
    #[cfg(test)]
    if TEST_ENABLED.get() { return true; }
    node().is_some()
}

/// `SV_DEVICE_TYPE=integrated|discrete|virtual|cpu`: a test-only override of
/// the `deviceType` reported. Mesa 26.2's Zink rejects a CPU-type device (this
/// ICD over lavapipe) unless `LIBGL_ALWAYS_SOFTWARE` is set, which also pushes
/// EGL and GBM onto software paths; a KWin-on-GBM test over lavapipe needs the
/// device to say "integrated". Unset: the GPU side's answer, unchanged.
pub fn device_type_override() -> Option<i32> {
    static T: std::sync::OnceLock<Option<i32>> = std::sync::OnceLock::new();
    *T.get_or_init(|| match std::env::var("SV_DEVICE_TYPE").ok()?.as_str() {
        "" => None,
        "integrated" => Some(1),
        "discrete" => Some(2),
        "virtual" => Some(3),
        "cpu" => Some(4),
        other => {
            eprintln!("sv-icd: SV_DEVICE_TYPE={other:?} is not integrated|discrete|virtual|cpu; ignored");
            None
        }
    })
}

/// What a shared allocation's token says (`sv_drm::token::Token`, kept here in
/// a form that also builds for Windows). `record` is the fence record of the
/// GPU session that made it (§104.11), empty for a version-1 token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedToken {
    pub size: u64,
    pub nonce: [u8; 16],
    pub name: String,
    pub record: String,
}

/// The token file behind a shared `VkDeviceMemory` (§104.11).
///
/// One per allocation, made once: every `vkGetMemoryFdKHR` hands out a `dup`
/// of it, as a kernel dma-buf export hands out another fd for the same
/// buffer, so the sync page every holder maps is the same page.
pub enum TokenFile {
    /// Made by this process: its sync page is writable here. An import of a
    /// token this process made (KWin renders into an EGLImage of its own GBM
    /// buffer) shares the creator's handle.
    #[cfg(target_os = "linux")]
    Own(std::sync::Arc<sv_drm::token::Created>),
    /// Made by another process: the imported fd, kept (the specification
    /// gives it to the implementation), and its sync page, read-only.
    #[cfg(target_os = "linux")]
    Foreign { fd: std::os::fd::OwnedFd, page: Option<sv_drm::token::SyncPage> },
    /// No token file (not Linux).
    None,
}

impl TokenFile {
    /// Say that GPU work `seq` wrote the allocation, if this process can.
    pub fn mark(&self, seq: u64) {
        #[cfg(target_os = "linux")]
        if let TokenFile::Own(c) = self {
            c.mark(seq);
        }
        let _ = seq;
    }

    /// The work a user of a foreign allocation must wait for: the number its
    /// creator last stored. 0 for our own, or a version-1 token.
    pub fn foreign_seq(&self) -> u64 {
        match self {
            #[cfg(target_os = "linux")]
            TokenFile::Foreign { page: Some(p), .. } => p.load(),
            _ => 0,
        }
    }

    pub fn is_own(&self) -> bool {
        #[cfg(target_os = "linux")]
        if let TokenFile::Own(_) = self {
            return true;
        }
        false
    }

    /// A new fd for `vkGetMemoryFdKHR`; the caller owns it.
    pub fn export(&self) -> Option<i32> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::IntoRawFd;
            let r = match self {
                TokenFile::Own(c) => c.dup(),
                TokenFile::Foreign { fd, .. } => fd.try_clone(),
                TokenFile::None => return None,
            };
            return match r {
                Ok(fd) => Some(fd.into_raw_fd()),
                Err(e) => {
                    eprintln!("sv-icd: vkGetMemoryFdKHR: dup: {e}");
                    None
                }
            };
        }
        #[allow(unreachable_code)]
        None
    }
}

/// A shared `VkDeviceMemory` (§104): what its token says, and its file.
pub struct SharedMem {
    pub token: SharedToken,
    pub file: TokenFile,
}

/// The tokens this process made, by section name, so that an import of one
/// of them writes the creator's sync page (see [`TokenFile::Own`]). Weak: a
/// token goes with the last memory that holds it.
#[cfg(target_os = "linux")]
static OWN_TOKENS: std::sync::Mutex<Option<std::collections::HashMap<String, std::sync::Weak<sv_drm::token::Created>>>> =
    std::sync::Mutex::new(None);

/// The token file for a new exportable allocation.
#[cfg(target_os = "linux")]
pub fn make_token(t: &SharedToken) -> TokenFile {
    let tok = sv_drm::token::Token { size: t.size, nonce: t.nonce, name: t.name.clone(), record: t.record.clone() };
    match sv_drm::token::create(&tok) {
        Ok(c) => {
            let c = std::sync::Arc::new(c);
            let mut g = OWN_TOKENS.lock().unwrap_or_else(|e| e.into_inner());
            let m = g.get_or_insert_with(Default::default);
            m.retain(|_, w| w.strong_count() > 0);
            m.insert(t.name.clone(), std::sync::Arc::downgrade(&c));
            TokenFile::Own(c)
        }
        Err(e) => {
            eprintln!("sv-icd: the token for {}: {e}", t.name);
            TokenFile::None
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn make_token(_t: &SharedToken) -> TokenFile {
    TokenFile::None
}

/// Take an imported fd (the specification gives it to the implementation on
/// success; on failure the caller still owns it, so it is only borrowed until
/// [`adopt`] is called).
#[cfg(target_os = "linux")]
pub fn adopt(fd: i32, t: &SharedToken) -> TokenFile {
    use std::os::fd::FromRawFd;
    // SAFETY: the application handed this descriptor over with a successful
    // import; nothing else closes it.
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let mine = OWN_TOKENS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(&t.name))
        .and_then(std::sync::Weak::upgrade);
    if let Some(c) = mine {
        return TokenFile::Own(c);
    }
    let page = match sv_drm::token::SyncPage::open(std::os::fd::AsFd::as_fd(&owned)) {
        Ok(p) => p,
        Err(e) => {
            sv_icd_entry::report_always(&format_args!("an imported dma-buf's sync page: {e}"));
            None
        }
    };
    TokenFile::Foreign { fd: owned, page }
}

#[cfg(not(target_os = "linux"))]
pub fn adopt(fd: i32, _t: &SharedToken) -> TokenFile {
    close_fd(fd);
    TokenFile::None
}

/// The token behind `fd`, which stays open.
#[cfg(target_os = "linux")]
pub fn read_token(fd: i32) -> Option<SharedToken> {
    if fd < 0 {
        return None;
    }
    // SAFETY: the application's descriptor, borrowed for the call only.
    let b = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    match sv_drm::token::read(b) {
        Ok(t) => Some(SharedToken { size: t.size, nonce: t.nonce, name: t.name, record: t.record }),
        Err(e) => {
            sv_icd_entry::report_always(&format_args!("an imported dma-buf fd is not ours: {e}"));
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn read_token(_fd: i32) -> Option<SharedToken> {
    None
}

/// Close a descriptor the application handed over.
pub fn close_fd(fd: i32) {
    #[cfg(unix)]
    if fd >= 0 {
        // SAFETY: the specification gives this descriptor to the
        // implementation on a successful import; nothing else closes it.
        unsafe { libc::close(fd) };
    }
    #[cfg(not(unix))]
    let _ = fd;
}

/// What the app side remembers about an image created through this path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DmabufImage {
    /// `VkImageDrmFormatModifierExplicitCreateInfoEXT`'s plane 0: offset and
    /// row pitch the importer asked for.
    pub explicit: Option<(u64, u64)>,
    /// Every plane's explicit `(offset, row pitch)`, plane 0 first; `explicit`
    /// is the first of them (§112: NV12 imported whole, two planes).
    pub planes: [(u64, u64); 3],
    pub nplanes: u8,
    /// Added to every bind of this image: an import whose plane 0 is not at
    /// the start of the dma-buf (an NV12 frame's chroma plane imported on
    /// its own as R8G8, §112), checked against the driver's layout.
    pub bind_offset: u64,
    /// Full backend image requirement, checked after applying the explicit
    /// layout translation before any bind reaches the real driver.
    pub required_size: u64,
    pub required_alignment: u64,
    /// The image's `VkFormat`, for its plane count.
    pub format: i32,
    /// Made with `VkExternalMemoryImageCreateInfo{DMA_BUF}`: bound to a
    /// shared section on the GPU side.
    pub external: bool,
}

/// Preserve the exact explicit plane offsets without wrapping arithmetic.
pub(crate) fn explicit_base(wanted: &[(u64, u64)], actual: &[(u64, u64)], alignment: u64) -> Option<u64> {
    if wanted.is_empty() || wanted.len() != actual.len() || alignment == 0 { return None; }
    let base = wanted[0].0.checked_sub(actual[0].0)?;
    (base % alignment == 0 && wanted.iter().zip(actual).all(|(w, a)| w.1 == a.1 && a.0.checked_add(base) == Some(w.0))).then_some(base)
}

impl DmabufImage {
    pub(crate) fn translated_offset(&self, offset: u64) -> Option<u64> { offset.checked_add(self.bind_offset) }
    pub(crate) fn binding_offset(&self, requested: u64, allocation_size: u64) -> Option<u64> {
        let offset = self.translated_offset(requested)?;
        (self.required_size > 0 && self.required_alignment > 0
            && offset % self.required_alignment == 0
            && offset.checked_add(self.required_size)? <= allocation_size).then_some(offset)
    }
}

/// What the app side remembers about a semaphore created exportable as a
/// sync fd, or imported from one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Semaphore {
    /// The queue of the last submit that signalled it, until an export
    /// consumes that signal.
    pub signal_queue: Option<u64>,
    /// The permanent payload was signaled by completed CPU-side acquisition.
    pub presignalled: bool,
    /// A temporary already-complete SYNC_FD import shadows the permanent
    /// payload until the next wait or copy-payload export restores it.
    pub temporary_presignalled: bool,
    /// Every local payload transition changes this revision, allowing failed
    /// submissions to roll back only their own still-current reservation.
    pub revision: u64,
}
impl Semaphore {
    pub fn advance_revision(&mut self) {
        self.revision = self.revision.checked_add(1).expect("semaphore revision exhausted");
    }
    pub fn consume_local_signal(&mut self) -> bool {
        if self.temporary_presignalled {
            self.temporary_presignalled = false;
            true
        } else {
            std::mem::take(&mut self.presignalled)
        }
    }
}

/// The implicit-sync bookkeeping of §104.11, app side.
///
/// Zink's implicit-sync export after a flush is, per exported image,
/// `vkGetSemaphoreFdKHR(SYNC_FD)` immediately followed by `vkGetMemoryFdKHR`
/// of the image's memory and `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` on that fd
/// (`zink_screen_import_dmabuf_semaphore`). The export numbers the work
/// ([`Fences::begin_mark`]); the memory fd that follows says which allocation
/// it wrote, and the mark goes to the GPU side then. Before a batch first uses
/// an imported image, Zink asks for the memory fd *without* an export in front
/// (`zink_screen_export_dmabuf_semaphore`, for `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`):
/// that is the moment to note what the next submit must wait for.
#[derive(Debug, Default)]
pub struct Fences {
    /// The session's fence record: `None` not asked yet, `Some(None)` the GPU
    /// side has none (the export then waits, as before §104.11).
    pub record: Option<Option<String>>,
    /// The last number given out.
    pub seq: u64,
    /// Exports whose memory fd has not come yet, one per thread: Zink asks
    /// for the sync fd and the memory fd one after the other on one thread,
    /// so two contexts flushing on two threads do not swap their numbers.
    pub pending: Vec<(std::thread::ThreadId, Mark)>,
    /// Exports another thread's submit sent before their memory fd came:
    /// the mark is out, but the sync page still wants the number.
    pub sent: Vec<(std::thread::ThreadId, Mark)>,
    /// What the next submit waits for: `(memory, record, seq)`.
    pub waits: Vec<(u64, String, u64)>,
}

/// One numbered export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    pub queue: u64,
    pub semaphore: u64,
    pub seq: u64,
}

impl Fences {
    /// Number the export of `semaphore`, last signalled on `queue`, on this
    /// thread. Returns this thread's export still pending before it (to be
    /// sent with no memory).
    pub fn begin_mark(&mut self, queue: u64, semaphore: u64) -> (Mark, Option<Mark>) {
        self.seq += 1;
        let m = Mark { queue, semaphore, seq: self.seq };
        let me = std::thread::current().id();
        let before = self.pending.iter().position(|(t, _)| *t == me).map(|i| self.pending.swap_remove(i).1);
        self.sent.retain(|(t, _)| *t != me);
        self.pending.push((me, m));
        (m, before)
    }

    /// This thread's last export, if its memory fd has not come yet, and
    /// whether its mark was already sent (by a flush on another thread).
    pub fn take_pending(&mut self) -> Option<(Mark, bool)> {
        let me = std::thread::current().id();
        if let Some(i) = self.pending.iter().position(|(t, _)| *t == me) {
            return Some((self.pending.swap_remove(i).1, false));
        }
        let i = self.sent.iter().position(|(t, _)| *t == me)?;
        Some((self.sent.swap_remove(i).1, true))
    }

    /// Every pending export, in number order, for a flush on this thread.
    /// Another thread's stays known as sent, for its memory fd to come.
    pub fn take_all_pending(&mut self) -> Vec<Mark> {
        let me = std::thread::current().id();
        self.sent.retain(|(t, _)| *t != me);
        let mut out = Vec::new();
        for (t, m) in std::mem::take(&mut self.pending) {
            if t != me {
                self.sent.retain(|(u, _)| *u != t);
                self.sent.push((t, m));
            }
            out.push(m);
        }
        out.sort_by_key(|m| m.seq);
        out
    }

    /// A wait for the next submit, once per memory: the larger number wins.
    pub fn add_wait(&mut self, memory: u64, record: &str, seq: u64) {
        if let Some(w) = self.waits.iter_mut().find(|w| w.0 == memory) {
            if seq > w.2 {
                w.1 = record.to_string();
                w.2 = seq;
            }
            return;
        }
        self.waits.push((memory, record.to_string(), seq));
    }
}

fn is_special_family(f: u32) -> bool {
    f == QUEUE_FAMILY_EXTERNAL || f == QUEUE_FAMILY_FOREIGN
}

/// A queue family ownership transfer from or to "outside" is a plain barrier
/// here: the GPU side's device is the only one that touches the memory, and
/// Intel's Windows driver has no foreign queue family to transfer to.
pub fn rewrite_families(src: &mut u32, dst: &mut u32) {
    if is_special_family(*src) || is_special_family(*dst) {
        *src = QUEUE_FAMILY_IGNORED;
        *dst = QUEUE_FAMILY_IGNORED;
    }
}

/// Make a `VkImageCreateInfo` one the GPU side can create: a modifier image
/// becomes LINEAR, a dma-buf image becomes a host-allocation one. `Ok(None)`
/// when this path has nothing to do with the image.
pub fn rewrite_image_create_info(ci: &mut p::VkImageCreateInfo) -> Result<Option<DmabufImage>, i32> {
    let mut img = DmabufImage::default();
    img.format = ci.format.0;
    let mut ours = false;
    if ci.tiling.0 == TILING_DRM_FORMAT_MODIFIER {
        ours = true;
        let mut linear = false;
        for l in &ci.p_next {
            match l {
                p::PNextLink::VkImageDrmFormatModifierListCreateInfoEXT(x) => {
                    linear |= x.p_drm_format_modifiers.contains(&DRM_FORMAT_MOD_LINEAR);
                }
                p::PNextLink::VkImageDrmFormatModifierExplicitCreateInfoEXT(x) => {
                    let n = x.p_plane_layouts.len();
                    if x.drm_format_modifier != DRM_FORMAT_MOD_LINEAR || n == 0 || n > 3 || n as u32 != plane_count(ci.format.0) {
                        return Err(VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT);
                    }
                    for (i, pl) in x.p_plane_layouts.iter().enumerate() {
                        img.planes[i] = (pl.offset, pl.row_pitch);
                    }
                    img.nplanes = n as u8;
                    img.explicit = Some(img.planes[0]);
                    linear = true;
                }
                _ => {}
            }
        }
        if !linear {
            return Err(VK_ERROR_FORMAT_NOT_SUPPORTED);
        }
        ci.tiling = p::VkImageTiling(TILING_LINEAR);
    }
    ci.p_next.retain(|l| {
        !matches!(
            l,
            p::PNextLink::VkImageDrmFormatModifierListCreateInfoEXT(_)
                | p::PNextLink::VkImageDrmFormatModifierExplicitCreateInfoEXT(_)
        )
    });
    let mut drop_external = false;
    for l in ci.p_next.iter_mut() {
        if let p::PNextLink::VkExternalMemoryImageCreateInfo(x) = l {
            if x.handle_types.0 & DMA_BUF != 0 {
                x.handle_types.0 = HOST_ALLOCATION;
                img.external = true;
                ours = true;
                ci.tiling = p::VkImageTiling(TILING_LINEAR);
            } else {
                // Another handle type (OPAQUE_FD) cannot be exported here and
                // the driver behind the GPU side need not know its name.
                drop_external = true;
            }
        }
    }
    if drop_external {
        ci.p_next.retain(|l| !matches!(l, p::PNextLink::VkExternalMemoryImageCreateInfo(x) if x.handle_types.0 != HOST_ALLOCATION));
    }
    // This adapter binds one complete backing allocation. Plane-specific
    // requirements and VkBindImagePlaneMemoryInfo are not implemented.
    if ours && ci.flags.0 & 0x0000_0200 != 0 { // VK_IMAGE_CREATE_DISJOINT_BIT
        return Err(VK_ERROR_FORMAT_NOT_SUPPORTED);
    }
    Ok(ours.then_some(img))
}

/// What [`rewrite_image_format_info`] found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FormatQuery {
    /// The application asked about `DMA_BUF`, which was asked of the driver as
    /// `HOST_ALLOCATION`; the answer is rewritten by [`answer_image_format`].
    pub dma_buf: bool,
}

/// Make a `VkPhysicalDeviceImageFormatInfo2` the GPU side's driver can answer
/// for what this path will actually create. `Err` is the answer itself.
pub fn rewrite_image_format_info(info: &mut p::VkPhysicalDeviceImageFormatInfo2) -> Result<FormatQuery, i32> {
    let mut q = FormatQuery::default();
    let modifier = info.tiling.0 == TILING_DRM_FORMAT_MODIFIER;
    if info.tiling.0 == TILING_DRM_FORMAT_MODIFIER {
        let m = info.p_next.iter().find_map(|l| match l {
            p::PNextLink::VkPhysicalDeviceImageDrmFormatModifierInfoEXT(x) => Some(x.drm_format_modifier),
            _ => None,
        });
        if m != Some(DRM_FORMAT_MOD_LINEAR) {
            return Err(VK_ERROR_FORMAT_NOT_SUPPORTED);
        }
        info.tiling = p::VkImageTiling(TILING_LINEAR);
    }
    info.p_next.retain(|l| !matches!(l, p::PNextLink::VkPhysicalDeviceImageDrmFormatModifierInfoEXT(_)));
    let mut strip = false;
    for l in info.p_next.iter_mut() {
        if let p::PNextLink::VkPhysicalDeviceExternalImageFormatInfo(x) = l {
            match x.handle_type.0 as u32 {
                0 => strip = true,
                DMA_BUF => {
                    x.handle_type = p::VkExternalMemoryHandleTypeFlagBits(HOST_ALLOCATION as i32);
                    q.dma_buf = true;
                    info.tiling = p::VkImageTiling(TILING_LINEAR);
                }
                _ => return Err(VK_ERROR_FORMAT_NOT_SUPPORTED),
            }
        }
    }
    if strip {
        info.p_next.retain(|l| !matches!(l, p::PNextLink::VkPhysicalDeviceExternalImageFormatInfo(_)));
    }
    if (modifier || q.dma_buf) && info.flags.0 & 0x0000_0200 != 0 {
        return Err(VK_ERROR_FORMAT_NOT_SUPPORTED);
    }
    Ok(q)
}

/// Make sure the request's output chain asks the driver for its external
/// answer, which [`answer_image_format`] reads.
pub fn want_external_answer(out: &mut p::VkImageFormatProperties2) {
    if !out.p_next.iter().any(|l| matches!(l, p::PNextLink::VkExternalImageFormatProperties(_))) {
        out.p_next.push(p::PNextLink::VkExternalImageFormatProperties(Box::default()));
    }
}

/// The application's answer for a `DMA_BUF` query, from the driver's answer
/// for `HOST_ALLOCATION`: exportable and importable as a dma-buf exactly when
/// the driver can import host memory for such an image.
pub fn answer_image_format(out: &mut p::VkImageFormatProperties2) -> Result<(), i32> {
    let ext = out.p_next.iter_mut().find_map(|l| match l {
        p::PNextLink::VkExternalImageFormatProperties(x) => Some(x),
        _ => None,
    });
    let Some(ext) = ext else { return Err(VK_ERROR_FORMAT_NOT_SUPPORTED) };
    if ext.external_memory_properties.external_memory_features.0 & IMPORTABLE == 0 {
        return Err(VK_ERROR_FORMAT_NOT_SUPPORTED);
    }
    let m = &mut ext.external_memory_properties;
    m.external_memory_features = p::VkExternalMemoryFeatureFlags(MEMORY_FEATURES);
    m.export_from_imported_handle_types = p::VkExternalMemoryHandleTypeFlags(DMA_BUF);
    m.compatible_handle_types = p::VkExternalMemoryHandleTypeFlags(DMA_BUF);
    Ok(())
}

/// `VkPhysicalDeviceDrmPropertiesEXT` for the node.
pub fn drm_properties(major: u32, minor: u32) -> p::VkPhysicalDeviceDrmPropertiesEXT {
    let mut d = p::VkPhysicalDeviceDrmPropertiesEXT::default();
    d.has_primary = false;
    d.has_render = true;
    d.render_major = i64::from(major);
    d.render_minor = i64::from(minor);
    d
}

/// Drop every wait on a pre-signalled semaphore from one batch, keeping the
/// parallel stage-mask (and timeline value) arrays in step.
pub fn drop_presignalled_waits(s: &mut p::VkSubmitInfo, mut take: impl FnMut(u64) -> bool) {
    let mut i = 0;
    while i < s.p_wait_semaphores.len() {
        if take(s.p_wait_semaphores[i].0) {
            s.p_wait_semaphores.remove(i);
            if i < s.p_wait_dst_stage_mask.len() {
                s.p_wait_dst_stage_mask.remove(i);
            }
            for l in s.p_next.iter_mut() {
                if let p::PNextLink::VkTimelineSemaphoreSubmitInfo(t) = l
                    && i < t.p_wait_semaphore_values.len()
                {
                    t.p_wait_semaphore_values.remove(i);
                    t.wait_semaphore_value_count = t.p_wait_semaphore_values.len() as u32;
                }
                if let p::PNextLink::VkDeviceGroupSubmitInfo(group) = l
                    && i < group.p_wait_semaphore_device_indices.len()
                {
                    group.p_wait_semaphore_device_indices.remove(i);
                    group.wait_semaphore_count = group.p_wait_semaphore_device_indices.len() as u32;
                }
            }
        } else {
            i += 1;
        }
    }
    s.wait_semaphore_count = s.p_wait_semaphores.len() as u32;
}

// -- vkGetPhysicalDeviceFormatProperties2 -------------------------------------
//
// `VkDrmFormatModifierPropertiesListEXT` carries an output array inside an
// output chain, which the generated write-back cannot do (it would have to know
// the caller's capacity), so this one link is filled here, on the C side, after
// the generated entry point has filled everything else.

const STYPE_LIST: i32 = 1_000_158_000;
const STYPE_LIST2: i32 = 1_000_158_006;
const STYPE_FORMAT_PROPERTIES_3: i32 = 1_000_360_000;

/// The entry point the loader is given for `vkGetPhysicalDeviceFormatProperties2`
/// (and its KHR alias) while the path is on.
///
/// # Safety
/// The Vulkan contract: `out` is null or a valid `VkFormatProperties2` whose
/// chain is valid for writes.
pub unsafe extern "system" fn get_physical_device_format_properties2(
    physical_device: sv_icd_entry::VkPhysicalDevice,
    format: sv_icd_entry::VkFormat,
    out: *mut sv_icd_entry::VkFormatProperties2,
) {
    // SAFETY: forwarded with the caller's own arguments.
    unsafe { sv_icd_entry::sv_vk_get_physical_device_format_properties2(physical_device, format, out) };
    if out.is_null() {
        return;
    }
    // SAFETY: the caller's contract; every link is read by its sType.
    unsafe {
        let linear = (*out).format_properties.linear_tiling_features.0;
        let mut linear2 = u64::from(linear);
        let mut p = (*out).p_next as *mut sv_icd_entry::VkBaseOutStructure;
        let mut guard = 0;
        while !p.is_null() && guard < 64 {
            if (*p).s_type.0 == STYPE_FORMAT_PROPERTIES_3 {
                linear2 = (*(p as *mut sv_icd_entry::VkFormatProperties3)).linear_tiling_features.0;
            }
            p = (*p).p_next;
            guard += 1;
        }
        let mut p = (*out).p_next as *mut sv_icd_entry::VkBaseOutStructure;
        let mut guard = 0;
        while !p.is_null() && guard < 64 {
            match (*p).s_type.0 {
                STYPE_LIST => {
                    let l = p as *mut sv_icd_entry::VkDrmFormatModifierPropertiesListEXT;
                    let have = u32::from(linear != 0);
                    if (*l).p_drm_format_modifier_properties.is_null() {
                        (*l).drm_format_modifier_count = have;
                    } else {
                        let n = have.min((*l).drm_format_modifier_count);
                        if n == 1 {
                            *(*l).p_drm_format_modifier_properties = sv_icd_entry::VkDrmFormatModifierPropertiesEXT {
                                drm_format_modifier: DRM_FORMAT_MOD_LINEAR,
                                drm_format_modifier_plane_count: plane_count(format.0),
                                drm_format_modifier_tiling_features: sv_icd_entry::VkFormatFeatureFlags(linear),
                            };
                        }
                        (*l).drm_format_modifier_count = n;
                    }
                }
                STYPE_LIST2 => {
                    let l = p as *mut sv_icd_entry::VkDrmFormatModifierPropertiesList2EXT;
                    let have = u32::from(linear2 != 0);
                    if (*l).p_drm_format_modifier_properties.is_null() {
                        (*l).drm_format_modifier_count = have;
                    } else {
                        let n = have.min((*l).drm_format_modifier_count);
                        if n == 1 {
                            *(*l).p_drm_format_modifier_properties = sv_icd_entry::VkDrmFormatModifierProperties2EXT {
                                drm_format_modifier: DRM_FORMAT_MOD_LINEAR,
                                drm_format_modifier_plane_count: plane_count(format.0),
                                drm_format_modifier_tiling_features: sv_icd_entry::VkFormatFeatureFlags2(linear2),
                            };
                        }
                        (*l).drm_format_modifier_count = n;
                    }
                }
                _ => {}
            }
            p = (*p).p_next;
            guard += 1;
        }
    }
}

/// The format-properties entry point to hand the loader for `name`, when the
/// path is on.
pub fn proc_override(name: &str) -> Option<*const core::ffi::c_void> {
    if !enabled() {
        return None;
    }
    match name {
        "vkGetPhysicalDeviceFormatProperties2" | "vkGetPhysicalDeviceFormatProperties2KHR" => {
            Some(get_physical_device_format_properties2 as *const core::ffi::c_void)
        }
        _ => None,
    }
}

/// Strip the modifier-list links from a format query's output chain: they
/// are filled here, not by the driver behind the GPU side.
pub fn strip_modifier_lists(out: &mut p::VkFormatProperties2) {
    out.p_next.retain(|l| {
        !matches!(
            l,
            p::PNextLink::VkDrmFormatModifierPropertiesListEXT(_) | p::PNextLink::VkDrmFormatModifierPropertiesList2EXT(_)
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(tiling: i32, links: Vec<p::PNextLink>) -> p::VkImageCreateInfo {
        let mut ci = p::VkImageCreateInfo::default();
        ci.tiling = p::VkImageTiling(tiling);
        ci.p_next = links;
        ci
    }

    fn list(mods: &[u64]) -> p::PNextLink {
        let mut l = p::VkImageDrmFormatModifierListCreateInfoEXT::default();
        l.p_drm_format_modifiers = mods.to_vec();
        l.drm_format_modifier_count = mods.len() as u32;
        p::PNextLink::VkImageDrmFormatModifierListCreateInfoEXT(Box::new(l))
    }

    fn external(types: u32) -> p::PNextLink {
        let mut e = p::VkExternalMemoryImageCreateInfo::default();
        e.handle_types = p::VkExternalMemoryHandleTypeFlags(types);
        p::PNextLink::VkExternalMemoryImageCreateInfo(Box::new(e))
    }

    /// What Zink's GBM allocation sends: a modifier list with LINEAR, and
    /// DMA_BUF export. The GPU side sees a LINEAR host-allocation image and no
    /// modifier struct.
    #[test]
    fn a_gbm_allocation_becomes_a_linear_host_allocation_image() {
        let mut ci = image(TILING_DRM_FORMAT_MODIFIER, vec![list(&[0x0100_0000_0000_0001, 0]), external(DMA_BUF | 1)]);
        let img = rewrite_image_create_info(&mut ci).unwrap().unwrap();
        assert!(img.external);
        assert_eq!(img.explicit, None);
        assert_eq!(ci.tiling.0, TILING_LINEAR);
        assert_eq!(ci.p_next.len(), 1);
        assert!(matches!(&ci.p_next[0], p::PNextLink::VkExternalMemoryImageCreateInfo(x) if x.handle_types.0 == HOST_ALLOCATION));
    }

    #[test]
    fn an_import_keeps_its_plane_layout_and_a_tiled_one_is_refused() {
        let mut ex = p::VkImageDrmFormatModifierExplicitCreateInfoEXT::default();
        ex.drm_format_modifier = 0;
        ex.drm_format_modifier_plane_count = 1;
        let mut pl = p::VkSubresourceLayout::default();
        pl.row_pitch = 5120;
        ex.p_plane_layouts = vec![pl];
        let link = p::PNextLink::VkImageDrmFormatModifierExplicitCreateInfoEXT(Box::new(ex.clone()));
        let mut ci = image(TILING_DRM_FORMAT_MODIFIER, vec![link, external(DMA_BUF)]);
        let img = rewrite_image_create_info(&mut ci).unwrap().unwrap();
        assert_eq!(img.explicit, Some((0, 5120)));

        ex.drm_format_modifier = 0x0100_0000_0000_0001; // I915_FORMAT_MOD_X_TILED
        let link = p::PNextLink::VkImageDrmFormatModifierExplicitCreateInfoEXT(Box::new(ex));
        let mut ci = image(TILING_DRM_FORMAT_MODIFIER, vec![link]);
        assert_eq!(rewrite_image_create_info(&mut ci), Err(VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT));
        let mut ci = image(TILING_DRM_FORMAT_MODIFIER, vec![list(&[7])]);
        assert_eq!(rewrite_image_create_info(&mut ci), Err(VK_ERROR_FORMAT_NOT_SUPPORTED));
    }

    #[test]
    fn disjoint_adapter_images_and_format_queries_are_refused() {
        for tiling in [TILING_DRM_FORMAT_MODIFIER, TILING_LINEAR] {
            let mut ci = image(tiling, vec![list(&[DRM_FORMAT_MOD_LINEAR]), external(DMA_BUF)]);
            ci.flags = p::VkImageCreateFlags(0x0000_0200);
            assert_eq!(rewrite_image_create_info(&mut ci), Err(VK_ERROR_FORMAT_NOT_SUPPORTED));
        }
        let mut ci = image(TILING_LINEAR, vec![]);
        ci.flags = p::VkImageCreateFlags(0x0000_0200);
        assert!(rewrite_image_create_info(&mut ci).unwrap().is_none());

        let mut info = p::VkPhysicalDeviceImageFormatInfo2::default();
        info.tiling = p::VkImageTiling(TILING_LINEAR);
        info.flags = p::VkImageCreateFlags(0x0000_0200);
        let mut external = p::VkPhysicalDeviceExternalImageFormatInfo::default();
        external.handle_type = p::VkExternalMemoryHandleTypeFlagBits(DMA_BUF as i32);
        info.p_next.push(p::PNextLink::VkPhysicalDeviceExternalImageFormatInfo(Box::new(external)));
        assert_eq!(rewrite_image_format_info(&mut info), Err(VK_ERROR_FORMAT_NOT_SUPPORTED));

        let mut info = p::VkPhysicalDeviceImageFormatInfo2::default();
        info.tiling = p::VkImageTiling(TILING_DRM_FORMAT_MODIFIER);
        info.flags = p::VkImageCreateFlags(0x0000_0200);
        info.p_next.push(p::PNextLink::VkPhysicalDeviceImageDrmFormatModifierInfoEXT(Box::default()));
        assert_eq!(rewrite_image_format_info(&mut info), Err(VK_ERROR_FORMAT_NOT_SUPPORTED));
    }

    #[test]
    fn an_ordinary_image_is_left_alone() {
        let mut ci = image(0, vec![]);
        assert_eq!(rewrite_image_create_info(&mut ci), Ok(None));
        assert_eq!(ci.tiling.0, 0);
        // An OPAQUE_FD-only image loses the struct (nothing can export it),
        // but stays the application's tiling.
        let mut ci = image(0, vec![external(1)]);
        assert_eq!(rewrite_image_create_info(&mut ci), Ok(None));
        assert!(ci.p_next.is_empty());
    }

    #[test]
    fn a_dma_buf_format_query_is_asked_as_host_allocation_and_answered_as_dma_buf() {
        let mut info = p::VkPhysicalDeviceImageFormatInfo2::default();
        info.tiling = p::VkImageTiling(TILING_DRM_FORMAT_MODIFIER);
        let mut m = p::VkPhysicalDeviceImageDrmFormatModifierInfoEXT::default();
        m.drm_format_modifier = 0;
        let mut e = p::VkPhysicalDeviceExternalImageFormatInfo::default();
        e.handle_type = p::VkExternalMemoryHandleTypeFlagBits(DMA_BUF as i32);
        info.p_next = vec![
            p::PNextLink::VkPhysicalDeviceImageDrmFormatModifierInfoEXT(Box::new(m.clone())),
            p::PNextLink::VkPhysicalDeviceExternalImageFormatInfo(Box::new(e.clone())),
        ];
        let q = rewrite_image_format_info(&mut info).unwrap();
        assert!(q.dma_buf);
        assert_eq!(info.tiling.0, TILING_LINEAR);
        assert_eq!(info.p_next.len(), 1);
        assert!(matches!(&info.p_next[0], p::PNextLink::VkPhysicalDeviceExternalImageFormatInfo(x) if x.handle_type.0 == HOST_ALLOCATION as i32));

        let mut out = p::VkImageFormatProperties2::default();
        want_external_answer(&mut out);
        want_external_answer(&mut out);
        assert_eq!(out.p_next.len(), 1);
        // The driver said "not importable": the dma-buf answer is no.
        assert_eq!(answer_image_format(&mut out.clone()), Err(VK_ERROR_FORMAT_NOT_SUPPORTED));
        if let p::PNextLink::VkExternalImageFormatProperties(x) = &mut out.p_next[0] {
            x.external_memory_properties.external_memory_features = p::VkExternalMemoryFeatureFlags(IMPORTABLE);
            x.external_memory_properties.compatible_handle_types = p::VkExternalMemoryHandleTypeFlags(HOST_ALLOCATION);
        }
        answer_image_format(&mut out).unwrap();
        let p::PNextLink::VkExternalImageFormatProperties(x) = &out.p_next[0] else { unreachable!() };
        assert_eq!(x.external_memory_properties.compatible_handle_types.0, DMA_BUF);
        assert_eq!(x.external_memory_properties.export_from_imported_handle_types.0, DMA_BUF);
        assert_eq!(x.external_memory_properties.external_memory_features.0, 0x7);

        // Another modifier, or another handle type: not supported.
        let mut info = p::VkPhysicalDeviceImageFormatInfo2::default();
        info.tiling = p::VkImageTiling(TILING_DRM_FORMAT_MODIFIER);
        m.drm_format_modifier = 5;
        info.p_next = vec![p::PNextLink::VkPhysicalDeviceImageDrmFormatModifierInfoEXT(Box::new(m))];
        assert_eq!(rewrite_image_format_info(&mut info), Err(VK_ERROR_FORMAT_NOT_SUPPORTED));
        let mut info = p::VkPhysicalDeviceImageFormatInfo2::default();
        e.handle_type = p::VkExternalMemoryHandleTypeFlagBits(1);
        info.p_next = vec![p::PNextLink::VkPhysicalDeviceExternalImageFormatInfo(Box::new(e))];
        assert_eq!(rewrite_image_format_info(&mut info), Err(VK_ERROR_FORMAT_NOT_SUPPORTED));
    }

    #[test]
    fn foreign_and_external_families_become_ignored_and_others_stay() {
        let (mut s, mut d) = (QUEUE_FAMILY_FOREIGN, 0);
        rewrite_families(&mut s, &mut d);
        assert_eq!((s, d), (QUEUE_FAMILY_IGNORED, QUEUE_FAMILY_IGNORED));
        let (mut s, mut d) = (0, QUEUE_FAMILY_EXTERNAL);
        rewrite_families(&mut s, &mut d);
        assert_eq!((s, d), (QUEUE_FAMILY_IGNORED, QUEUE_FAMILY_IGNORED));
        let (mut s, mut d) = (0, 2);
        rewrite_families(&mut s, &mut d);
        assert_eq!((s, d), (0, 2));
    }

    #[test]
    fn a_presignalled_wait_is_dropped_with_its_stage_and_value() {
        let mut si = p::VkSubmitInfo::default();
        si.p_wait_semaphores = vec![p::VkSemaphore(1), p::VkSemaphore(2), p::VkSemaphore(3)];
        si.p_wait_dst_stage_mask = vec![p::VkPipelineStageFlags(10), p::VkPipelineStageFlags(20), p::VkPipelineStageFlags(30)];
        si.wait_semaphore_count = 3;
        let mut t = p::VkTimelineSemaphoreSubmitInfo::default();
        t.p_wait_semaphore_values = vec![100, 200, 300];
        t.wait_semaphore_value_count = 3;
        si.p_next = vec![p::PNextLink::VkTimelineSemaphoreSubmitInfo(Box::new(t))];
        drop_presignalled_waits(&mut si, |s| s == 2);
        assert_eq!(si.p_wait_semaphores, vec![p::VkSemaphore(1), p::VkSemaphore(3)]);
        assert_eq!(si.p_wait_dst_stage_mask, vec![p::VkPipelineStageFlags(10), p::VkPipelineStageFlags(30)]);
        assert_eq!(si.wait_semaphore_count, 2);
        let p::PNextLink::VkTimelineSemaphoreSubmitInfo(t) = &si.p_next[0] else { unreachable!() };
        assert_eq!(t.p_wait_semaphore_values, vec![100, 300]);
    }

    /// Zink's order: export, then the memory fd, on one thread. A second
    /// export on the same thread before the fd sends the first without a
    /// memory; another thread's flush sends this thread's mark but leaves its
    /// number for the memory fd that follows.
    #[test]
    fn an_export_is_paired_with_the_next_memory_fd_of_its_thread() {
        let mut f = Fences::default();
        let (a, before) = f.begin_mark(1, 10);
        assert_eq!((a.seq, before), (1, None));
        assert_eq!(f.take_pending(), Some((a, false)));
        assert_eq!(f.take_pending(), None);

        let (b, _) = f.begin_mark(1, 11);
        let (c, before) = f.begin_mark(1, 12);
        assert_eq!(before, Some(b));
        assert_eq!(c.seq, 3);

        // Another thread exports, then this thread flushes (a submit): both
        // marks go out; the other thread's memory fd still gets its number,
        // already sent.
        let f = std::sync::Mutex::new(f);
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<Mark>();
        std::thread::scope(|s| {
            let f = &f;
            s.spawn(move || {
                let (d, _) = f.lock().unwrap().begin_mark(2, 20);
                done_tx.send(d).unwrap();
                go_rx.recv().unwrap();
                assert_eq!(f.lock().unwrap().take_pending(), Some((d, true)));
            });
            let d = done_rx.recv().unwrap();
            let out = f.lock().unwrap().take_all_pending();
            assert_eq!(out.iter().map(|m| m.seq).collect::<Vec<_>>(), [3, d.seq]);
            assert_eq!(f.lock().unwrap().take_pending(), None, "this thread's own flushed mark is not reused");
            go_tx.send(()).unwrap();
        });
        let f = f.into_inner().unwrap();
        assert!(f.pending.is_empty() && f.sent.is_empty());
    }

    #[test]
    fn a_wait_is_kept_once_per_memory_with_the_larger_number() {
        let mut f = Fences::default();
        f.add_wait(7, "a", 3);
        f.add_wait(7, "b", 2);
        f.add_wait(8, "", 0);
        f.add_wait(7, "c", 5);
        assert_eq!(f.waits, vec![(7, "c".to_string(), 5), (8, String::new(), 0)]);
    }

    #[test]
    fn the_drm_properties_name_the_render_node_only() {
        let d = drm_properties(226, 128);
        assert!(!d.has_primary && d.has_render);
        assert_eq!((d.render_major, d.render_minor), (226, 128));
    }
}

#[cfg(test)]
mod explicit_layout_tests {
    use super::*;
    #[test]
    fn chroma_shift_preserves_layout_and_checks_the_complete_image_extent() {
        let wanted = [(8192, 256)];
        assert_eq!(explicit_base(&wanted, &[(0, 256)], 4096), Some(8192));
        let image = DmabufImage { bind_offset: 8192, required_size: 4096, required_alignment: 4096, ..Default::default() };
        assert_eq!(image.translated_offset(0), Some(8192));
        assert_eq!(image.binding_offset(0, 12288), Some(8192));
        assert_eq!(image.binding_offset(0, 12287), None);
        assert_eq!(image.binding_offset(1, 16384), None);
        assert_eq!(image.binding_offset(u64::MAX, u64::MAX), None);
        assert_eq!(image.translated_offset(u64::MAX), None);
    }
    #[test]
    fn all_planes_must_share_one_checked_shift_and_exact_pitches() {
        let actual = [(0, 256), (8192, 256)];
        assert_eq!(explicit_base(&[(4096,256),(12288,256)], &actual, 4096), Some(4096));
        assert_eq!(explicit_base(&[(4096,256),(12289,256)], &actual, 4096), None);
        assert_eq!(explicit_base(&[(4096,256),(12288,128)], &actual, 4096), None);
        assert_eq!(explicit_base(&[(1,256)], &[(2,256)], 1), None);
        assert_eq!(explicit_base(&[(1,256),(0,256)], &[(0,256),(u64::MAX,256)], 1), None);
        assert_eq!(explicit_base(&[(0,256)], &[(0,256)], 0), None);
        let image = DmabufImage { bind_offset: u64::MAX - 4095, required_size: 4096, required_alignment: 1, ..Default::default() };
        assert_eq!(image.binding_offset(0, u64::MAX), None);
    }
}
