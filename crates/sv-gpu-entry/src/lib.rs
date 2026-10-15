//! The GPU side's C boundary: **wire → C**, and the driver call across it.
//!
//! docs/design.md §7.2's pass taxonomy has three of the four corners of "which side"
//! × "which direction" and is missing this one. The previous stage wrote it by
//! hand for about forty commands and recorded that it would not scale to 239;
//! this crate is that finding acted on. The generator is
//! `sv_codegen::emit::gpu_entry`, whose module header carries the argument.
//!
//! # What is hand-written here
//!
//! [`Raise`], and nothing else. It is docs/design.md §7.2's **pass 4**, which that
//! section strikes out with the note that owned Rust values make it a no-op.
//! That is true of *decoding* and false of *raising*: a C
//! `VkGraphicsPipelineCreateInfo` holds eight pointers to sub-structures that
//! must outlive the call, and producing them is precisely gfxstream's
//! pool-allocated `deepcopy`.
//!
//! # Where the `unsafe` is, and where it is not
//!
//! The **generated** `raise_*` half — much the larger part of this crate —
//! contains no `unsafe` at all, which was not obvious in advance. Creating a raw
//! pointer is safe; only dereferencing one is not, and raising never
//! dereferences: it allocates owned storage and takes its address. In the
//! generated file the `unsafe` is one line per command, the call through the
//! driver's function pointer, plus the `transmute` that gives a
//! `vkGetDeviceProcAddr` result its type.
//!
//! That is a better ratio than the app side manages, and the asymmetry is real
//! rather than an artefact: lowering has to *read* the application's memory,
//! raising only has to *produce* memory of its own.
//!
//! [`Raise`] itself does contain `unsafe`, and the reason is worth reading: the
//! safe version was unsound and only Miri could tell.

// The generated code names Vulkan types and constants exactly as the registry
// spells them, for the reason `sv-icd-entry` gives: a name that greps against
// the specification is worth more than one that satisfies a lint.
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(unused_parens)]
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::useless_transmute)]
#![allow(clippy::unnecessary_cast)]
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::redundant_field_names)]
// `DriverFns` is 223 `Option<fn>` fields. `#[derive(Default)]` would say the
// same thing, and the generator emits the explicit impl because an emitter that
// chooses between a derive and an impl depending on whether every field happens
// to be `Default` is one more thing to get right. `sv-proto` makes the same
// trade for the same reason.
#![allow(clippy::derivable_impls)]
// Shape-dependent style lints, allowed for the reason `sv-icd-entry`'s header
// gives at length: every one of them asks for a *different spelling depending on
// what a field happens to be*, so satisfying them means the emitter deciding
// between two forms, which is one more thing for a generator to get wrong in the
// crate where a mistake is a driver reading the wrong offset. The generated code
// uses one spelling that is correct in every position it is spliced into.
#![allow(clippy::borrow_deref_ref)]
#![allow(clippy::explicit_auto_deref)]
#![allow(clippy::manual_clamp)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::redundant_locals)]
#![allow(clippy::vec_init_then_push)]

use core::any::Any;
use core::ffi::{c_char, c_void};

// The C type layer, shared with the app side rather than generated twice. Two
// generated copies of `#[repr(C)] struct VkBufferCreateInfo` would be two places
// for a layout to be wrong, and the failure mode is a driver reading a field
// from the wrong offset with no diagnostic whatsoever.
pub use sv_icd_entry::*;

/// The pool a raised value's pointers point into: docs/design.md §7.2's pass 4.
///
/// # `Vec<Box<T>>` is the obvious design and it is unsound. Miri said so.
///
/// The first version of this type was `Vec<Box<dyn Any>>`: `Box::new(v)`, take
/// `&*b`, push the box, hand back the pointer. It has the two properties raising
/// needs — a stable address and correct alignment — it is entirely safe Rust,
/// and it is **wrong**, for a reason that is invisible without a borrow checker
/// that models provenance:
///
/// > `<128748> was later invalidated at offsets [0x0..0x8] by a Unique retag`
/// > — `cargo miri test -p sv-gpu-entry`
///
/// Moving a `Box` retags its pointee as `Unique`, which invalidates every
/// pointer previously derived from it. Pushing the box into the vector is a
/// move; so is every reallocation of that vector afterwards. So the pointers
/// this pool hands out are invalidated at the moment they are stored, and again
/// each time the pool grows — while continuing to point at the right bytes and
/// to work perfectly on real hardware, which is precisely the failure mode Miri
/// exists to catch and testing cannot.
///
/// `Box::into_raw` is the fix and the standard arena idiom: after it there is no
/// `Box` left to move, the allocation is owned by a raw pointer, and nothing
/// retags anything until `Drop` reclaims it. The cost is that this module has
/// `unsafe` in it, which the generated `raise_*` code still does not —
/// docs/design.md §7.3's "confined to modules with written invariants", and the
/// invariant is stated on [`Raise::one`].
///
/// # Why not a bump allocator
///
/// It would be faster and it is what gfxstream uses. It would also need its own
/// alignment and lifetime reasoning for the same `unsafe` budget, and the
/// allocation traffic here is per *command parameter*, not per byte. Revisit
/// with a profile in hand; every call site is generated, so the change is local.
#[derive(Default)]
pub struct Raise {
    /// Owned allocations, freed in [`Drop`]. Never read through — the pointers
    /// handed out are the only access path, and they are valid for exactly as
    /// long as this pool is.
    keep: Vec<*mut dyn Any>,
    /// The guard bytes behind every allocation (waycompose-design.md §87):
    /// where they start, how many there are, and the type they follow.
    guards: Vec<(*const u8, usize, &'static str)>,
}

/// Bytes of canary behind every allocation a driver is handed (§87).
///
/// The guest's Intel driver ended the GPU side with `STATUS_HEAP_CORRUPTION`
/// (0xC0000374) in the middle of `vkGetPhysicalDeviceVideoFormatPropertiesKHR`,
/// at a different call each run, with arguments that the API dump of the same
/// calls on RADV and NVIDIA shows well formed. A driver that writes past a
/// struct or an array it was given corrupts this process's heap, and Windows
/// notices later, somewhere else. So every allocation the forwarder hands to a
/// driver carries this many bytes of [`CANARY`] after it: a write past the end
/// lands in memory this process owns, the session survives, and the check on
/// drop says which type it was. 256 because the largest plausible overrun is a
/// struct from another header version, and the largest field such a difference
/// has added is a `VkExtensionProperties`.
pub const GUARD: usize = 256;
/// The value the guard bytes hold until something writes them.
pub const CANARY: u8 = 0xA7;

#[repr(C)]
struct Guarded<T> {
    v: T,
    g: [u8; GUARD],
}

/// Say, once per type and at most 32 times in all, that a driver wrote past
/// something it was given.
fn report_overrun(what: &str, changed: usize, of: usize) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.iter().any(|s| s == what) || N.fetch_add(1, Ordering::Relaxed) >= 32 {
        return;
    }
    seen.push(what.to_string());
    eprintln!(
        "[gpu] GUARD: the driver wrote past {what}: {changed} of the {of} guard bytes after it \
         changed (waycompose-design.md §87); absorbed, not corrupted"
    );
}

/// Count the guard bytes at `p` that no longer hold [`CANARY`].
///
/// # Safety
///
/// `p` must be valid for reads of `n` initialised bytes.
unsafe fn changed(p: *const u8, n: usize) -> usize {
    // SAFETY: the caller's contract.
    unsafe { core::slice::from_raw_parts(p, n) }.iter().filter(|&&b| b != CANARY).count()
}

/// Give an output array [`GUARD`] bytes of canary after its last element, in
/// its spare capacity, before its pointer is handed to the driver (§87). Its
/// length is unchanged, so the driver is told the same count.
pub fn guard_spare<T>(v: &mut Vec<T>) {
    let size = core::mem::size_of::<T>().max(1);
    let slack = GUARD.div_ceil(size);
    v.reserve_exact(slack);
    // SAFETY: `reserve_exact` made at least `slack` elements of spare capacity
    // after `len`, which is `slack * size` writable bytes this vector owns.
    unsafe {
        core::ptr::write_bytes(v.as_mut_ptr().add(v.len()).cast::<u8>(), CANARY, slack * size)
    };
}

/// After the driver call: did it write past the array? Reported, never an
/// error — the bytes it wrote are in memory this process owns.
pub fn check_spare<T>(v: &[T], cmd: &str, param: &str) {
    let size = core::mem::size_of::<T>().max(1);
    let n = GUARD.div_ceil(size) * size;
    // SAFETY: `guard_spare` initialised these `n` bytes of the same vector's
    // spare capacity, and nothing has reallocated it since.
    let c = unsafe { changed(v.as_ptr().add(v.len()).cast::<u8>(), n) };
    if c != 0 {
        report_overrun(
            &format!("{cmd}'s {param} ({} x {})", v.len(), core::any::type_name::<T>()),
            c,
            n,
        );
    }
}

impl Raise {
    /// A pointer to one value, valid until this pool is dropped.
    ///
    /// # Invariant
    ///
    /// Every pointer returned by this type points at an allocation owned by
    /// `keep` and is valid for reads and writes of its type until `Drop` runs.
    /// Nothing else ever holds a reference to that allocation, so a driver
    /// writing through the pointer — which happens for `pNext` chain links on
    /// output queries — aliases nothing.
    pub fn one<T: Any>(&mut self, v: T) -> *mut T {
        let p = Box::into_raw(Box::new(Guarded { v, g: [CANARY; GUARD] }));
        // SAFETY: just created by `Box::into_raw`; both field pointers are into
        // that one live allocation.
        let (v, g) = unsafe { (&raw mut (*p).v, (&raw const (*p).g).cast::<u8>()) };
        self.guards.push((g, GUARD, core::any::type_name::<T>()));
        self.keep.push(p as *mut dyn Any);
        v
    }

    /// A pointer to the first of `v`'s elements, valid until this pool is
    /// dropped. Empty input gives a **dangling but aligned** pointer, which no
    /// generated call site uses: the emitter substitutes null for an empty
    /// array before calling this.
    pub fn slice<T: Any>(&mut self, mut v: Vec<T>) -> *mut T {
        guard_spare(&mut v);
        let size = core::mem::size_of::<T>().max(1);
        // SAFETY: `guard_spare` initialised these bytes; moving the vector into
        // a box below moves its header, not its buffer.
        let g = unsafe { v.as_ptr().add(v.len()).cast::<u8>() };
        self.guards.push((g, GUARD.div_ceil(size) * size, core::any::type_name::<T>()));
        // A boxed `Vec<T>` rather than a `Box<[T]>`, for a Rust reason and not a
        // Vulkan one: `*mut [T]` is a fat pointer and cannot be stored as
        // `*mut dyn Any`. What matters either way is that the *elements* live in
        // their own allocation, whose address does not move.
        let boxed: *mut Vec<T> = Box::into_raw(Box::new(v));
        // SAFETY: just created by `Box::into_raw`, so it is a valid, uniquely
        // owned `Vec<T>` and nothing else refers to it.
        let data = unsafe { (*boxed).as_mut_ptr() };
        self.keep.push(boxed as *mut dyn Any);
        data
    }

    /// A NUL-terminated copy of `bytes`.
    ///
    /// The wire never carries the terminator — it is a fact of the *format*, not
    /// of the value (docs/design.md §6.1) — so it is added here. An interior NUL is
    /// not rejected: it would truncate the string a driver sees, and a wire
    /// string containing one is a corrupt peer rather than a case to handle.
    pub fn cstring(&mut self, bytes: &[u8]) -> *mut c_char {
        let mut v = Vec::with_capacity(bytes.len() + 1);
        v.extend_from_slice(bytes);
        v.push(0);
        self.slice(v) as *mut c_char
    }

    /// A copy of `bytes` at eight-byte alignment.
    ///
    /// Eight rather than one because a sized `void*` in the registry is
    /// `pCode` (SPIR-V, `uint32_t`), `pInitialData` (a pipeline cache blob,
    /// which drivers read as words) or `pData`. Copying through `u64` words
    /// gives the alignment every one of them needs without a cast that assumes
    /// one, and costs one pass over bytes that were about to be copied anyway.
    pub fn blob(&mut self, bytes: &[u8]) -> *mut c_void {
        if bytes.is_empty() {
            return core::ptr::null_mut();
        }
        let mut words: Vec<u64> = Vec::with_capacity(bytes.len().div_ceil(8));
        for chunk in bytes.chunks(8) {
            let mut w = [0u8; 8];
            w[..chunk.len()].copy_from_slice(chunk);
            words.push(u64::from_ne_bytes(w));
        }
        self.slice(words) as *mut c_void
    }

    /// How many allocations this pool is holding, for tests.
    pub fn len(&self) -> usize {
        self.keep.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keep.is_empty()
    }
}

impl Drop for Raise {
    fn drop(&mut self) {
        for (g, n, ty) in self.guards.drain(..) {
            // SAFETY: each guard lies in an allocation `keep` still owns, and
            // was initialised when it was recorded.
            let c = unsafe { changed(g, n) };
            if c != 0 {
                report_overrun(&format!("a {ty}"), c, n);
            }
        }
        for p in self.keep.drain(..) {
            // SAFETY: every entry came from `Box::into_raw` in this type's own
            // methods, is dropped exactly once here, and nothing else owns it.
            unsafe { drop(Box::from_raw(p)) };
        }
    }
}

include!(concat!(env!("OUT_DIR"), "/sv_gpu_entry.rs"));

/// A [`GpuHost`] wearing the two traits the generated struct walkers ask for.
///
/// `sv_proto::handle_map_*` wants a `&mut dyn HandleMapper` and
/// `sv_proto::transform_*` wants a `&mut dyn MemoryTransform`; one host cannot
/// lend both at once, and it does not have to — the two walks are separate
/// statements. Two short-lived adapters over the same host is the whole of the
/// reconciliation, and it is here rather than generated because it is four lines
/// that do not depend on the registry.
pub struct Mapping<'a, H: GpuHost + ?Sized>(pub &'a mut H);

impl<H: GpuHost + ?Sized> sv_proto::HandleMapper for Mapping<'_, H> {
    fn map(&mut self, ty: sv_proto::HandleType, id: u64) -> sv_wire::Result<u64> {
        self.0.map_handle(ty, id)
    }
}

impl<H: GpuHost + ?Sized> sv_proto::MemoryTransform for Mapping<'_, H> {
    fn rewrite(&mut self, memory: u64, offset: u64) -> sv_wire::Result<(u64, u64)> {
        self.0.rewrite_memory(memory, offset)
    }
}

/// What [`GpuHost::lock_queue`] hands back: exclusive use of the device's one
/// `VkQueue` until it is dropped.
///
/// # Why the guard is a type here rather than a `MutexGuard` in the signature
///
/// The generated dispatch has to name the return type, and it is generated into
/// *this* crate, which has no idea what a GPU side's lock is made of — the
/// implementation in `sv-gpu-server` holds an `Arc<Mutex<()>>` shared by every
/// session of one device, and a test double holds nothing at all. An `Option`
/// lets both answer, and `None` means "this host has no queue to protect"
/// rather than "this host forgot to lock", because a host that forgot would not
/// compile.
///
/// # Why `()` and not the queue
///
/// The queue handle is the driver's and the generated code already holds it as a
/// raised parameter. What is being serialised is *use* of it, which is what
/// Vulkan's "externally synchronized" means, so there is nothing for the guard
/// to carry.
///
/// The field is never read, and that is the point: what it does happens in
/// `Drop`, at the generated `drop(__qg)`. `dead_code` is allowed rather than
/// worked around, because every way of "using" it would be a lie about what the
/// type is for.
pub struct QueueGuard<'a>(#[allow(dead_code)] Option<std::sync::MutexGuard<'a, ()>>);

impl<'a> QueueGuard<'a> {
    /// Hold the given lock. A poisoned lock is still honoured, for the reason
    /// `sv_gpu_server::Shared::objects` gives: what it protects is the right to
    /// call the driver, and a panic elsewhere does not make concurrent submits
    /// safe.
    pub fn held(m: &'a std::sync::Mutex<()>) -> QueueGuard<'a> {
        QueueGuard(Some(m.lock().unwrap_or_else(|e| e.into_inner())))
    }

    /// No queue to protect. For a host that never reaches a driver.
    pub fn none() -> QueueGuard<'static> {
        QueueGuard(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §87: a write inside what the driver was given touches no guard byte; a
    /// write past it — a struct, a pool slice, an output array — changes
    /// exactly the bytes written, in memory the pool or the vector owns.
    #[test]
    fn a_write_past_an_allocation_lands_in_its_guard() {
        let mut a = Raise::default();
        let p = a.one([0u64; 4]);
        let s = a.slice(vec![0u32; 3]);
        unsafe {
            (*p)[3] = u64::MAX;
            *s.add(2) = u32::MAX;
        }
        let quiet: Vec<usize> = a.guards.iter().map(|&(g, n, _)| unsafe { changed(g, n) }).collect();
        assert_eq!(quiet, [0, 0], "writes inside the allocations");
        unsafe {
            p.cast::<u8>().add(32).write(1);
            s.add(3).write(7); // one element past three
        }
        let loud: Vec<usize> = a.guards.iter().map(|&(g, n, _)| unsafe { changed(g, n) }).collect();
        assert_eq!(loud, [1, 4], "the byte past the struct, and all four bytes of the u32 past the slice");

        let mut v = vec![1u16, 2, 3];
        guard_spare(&mut v);
        assert_eq!(v.len(), 3, "the count the driver is told is unchanged");
        let n = GUARD.div_ceil(2) * 2;
        assert_eq!(unsafe { changed(v.as_ptr().add(3).cast(), n) }, 0);
        unsafe { v.as_mut_ptr().add(3).write(0x0101) };
        assert_eq!(unsafe { changed(v.as_ptr().add(3).cast(), n) }, 2);
        check_spare(&v, "vkTest", "pOut"); // reports; must not panic
    }

    /// The property the whole pass rests on: a pointer handed out by the pool is
    /// still the same memory after the pool has grown.
    ///
    /// A `Vec<Box<T>>` reallocating moves the *boxes*, not what they point at,
    /// and that distinction is the reason this type is `Vec<Box<_>>` rather than
    /// `Vec<_>`. Getting it wrong would produce a dangling pointer only once the
    /// vector happened to reallocate — a bug that appears at some particular
    /// number of command parameters and not before.
    #[test]
    fn a_pooled_pointer_survives_the_pool_growing() {
        let mut a = Raise::default();
        let first = a.one(1234u64);
        for i in 0..1000u64 {
            let _ = a.one(i);
        }
        assert_eq!(unsafe { *first }, 1234);
    }

    #[test]
    fn a_pooled_slice_is_contiguous_and_survives() {
        let mut a = Raise::default();
        let p = a.slice(vec![1u32, 2, 3, 4]);
        for _ in 0..1000 {
            let _ = a.one(0u8);
        }
        let seen = unsafe { core::slice::from_raw_parts(p, 4) };
        assert_eq!(seen, &[1, 2, 3, 4]);
    }

    #[test]
    fn a_raised_string_is_nul_terminated() {
        let mut a = Raise::default();
        let p = a.cstring(b"VK_KHR_swapchain");
        let s = unsafe { core::ffi::CStr::from_ptr(p) };
        assert_eq!(s.to_bytes(), b"VK_KHR_swapchain");
    }

    /// SPIR-V is read as `uint32_t` by every driver, so a blob that is not
    /// four-byte aligned is a fault on some architectures and a slow path on
    /// this one.
    #[test]
    fn a_raised_blob_is_eight_byte_aligned_and_keeps_its_bytes() {
        let mut a = Raise::default();
        let bytes: Vec<u8> = (0..37u8).collect();
        let p = a.blob(&bytes);
        assert_eq!(p as usize % 8, 0);
        let seen = unsafe { core::slice::from_raw_parts(p as *const u8, bytes.len()) };
        assert_eq!(seen, &bytes[..]);
    }

    #[test]
    fn an_empty_blob_is_null_rather_than_dangling() {
        let mut a = Raise::default();
        assert!(a.blob(&[]).is_null());
    }

    /// The two things `raise_*` reconstructs that the wire does not carry.
    ///
    /// `sType` is not a field of a wire struct at all — dropping it is what
    /// makes "a value with the wrong sType" unrepresentable (docs/design.md §7.2) —
    /// so raising has to put it back from the registry. And a `pNext` chain is a
    /// flat `Vec` on the wire and a linked list in C, built back to front
    /// because a link has to exist before the one in front of it can point at
    /// it.
    ///
    /// Getting `sType` wrong is not a subtle failure: the first generated run of
    /// this pass left it zero on the output struct of
    /// `vkGetPhysicalDeviceFeatures2` and the validation layer said so by name.
    #[test]
    fn raising_restores_the_stype_and_relinks_the_pnext_chain() {
        let mut a = Raise::default();
        let mut w = sv_proto::VkPhysicalDeviceFeatures2::default();
        w.p_next = vec![
            sv_proto::PNextLink::VkPhysicalDeviceVulkan11Features(Box::default()),
            sv_proto::PNextLink::VkPhysicalDeviceVulkan12Features(Box::default()),
        ];
        let c = raise_vk_physical_device_features2(&w, &mut a).unwrap();

        assert_eq!(
            c.s_type,
            VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
            "the sType comes from the registry, because the wire does not carry one"
        );

        // Walk the C chain and check the order survived.
        let mut seen = Vec::new();
        let mut p = c.p_next as *const VkBaseInStructure;
        while !p.is_null() {
            let (ty, next) = unsafe { ((*p).s_type, (*p).p_next) };
            seen.push(ty);
            p = next;
        }
        assert_eq!(
            seen,
            vec![
                VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES,
                VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES,
            ],
            "the chain is rebuilt in the order the wire listed it"
        );
    }

    /// `lower_full_*` reads what `lower_*` deliberately does not.
    ///
    /// This is the finding docs/design.md §0.4 item 1 records, made checkable. Both
    /// functions convert a C `VkPhysicalDeviceProperties` into the same wire
    /// type in the same direction, and they must disagree: on the *app* side the
    /// struct is the caller's uninitialised memory on the way out and reading it
    /// is undefined behaviour plus a small information leak (§6.1's three-state
    /// validity); on the *GPU* side it is the driver's answer and it is the only
    /// part that carries anything.
    ///
    /// Reusing the app side's version — which is what "the direction is the
    /// same, so the pass is reusable" argues for — reports every device as
    /// having an empty name and API version zero.
    #[test]
    fn the_gpu_sides_lowering_reads_a_returnedonly_struct_and_the_app_sides_does_not() {
        let mut c = VkPhysicalDeviceProperties::default();
        c.api_version = 0x0040_3000;
        c.vendor_id = 0x1002;
        c.device_name[0] = b'R' as _;
        c.device_name[1] = b'X' as _;

        let app = unsafe { lower_vk_physical_device_properties(&raw const c) }.unwrap();
        assert_eq!(app.api_version, 0, "the app side must not read a returnedonly member");
        assert_eq!(app.device_name[0], 0);

        let gpu = unsafe { lower_full_vk_physical_device_properties(&raw const c) }.unwrap();
        assert_eq!(gpu.api_version, 0x0040_3000, "the GPU side must read it — it is the answer");
        assert_eq!(gpu.vendor_id, 0x1002);
        assert_eq!(&gpu.device_name[..2], b"RX");
    }
}
