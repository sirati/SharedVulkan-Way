//! `sv-icd` — the shared_vulkan app side, as a Vulkan ICD the real loader can
//! enumerate.
//!
//! design.md D12: "A **Vulkan ICD**, not a layer. Loader interface **7**,
//! degrade to 5." A layer is never the terminator of a call chain, zero drivers
//! makes `vkCreateInstance` fail outright with
//! `VK_ERROR_INCOMPATIBLE_DRIVER`, and physical-device enumeration is aggregated
//! by the loader from ICD-returned handles a layer cannot mint.
//!
//! # Where the `unsafe` is
//!
//! Almost none of it is here. The `extern "system"` boundary, the C ABI types
//! and the lowering all live in `sv-icd-entry`, generated, in one mechanical
//! shape (design.md §7.3, R7). This crate contributes:
//!
//! - three exported entry points, below, which are the loader's own contract;
//! - [`arena::Mapping`], which is `memfd_create` + `mmap` and is one of the
//!   three modules design.md §7.3 names.
//!
//! Everything else — every `Icd` trait method — is safe Rust over owned values.
//!
//! # What this build does and does not do
//!
//! It loads, negotiates, enumerates one physical device, reports the memory
//! table of design.md §9.1, creates a device and a queue, allocates arena memory
//! and maps it to a real pointer into real shared pages. **It does not reach a
//! GPU**, because `sv-transport` and `sv-gpu-server` are not on master. Every
//! place that would have asked a GPU says so, and
//! [`sv_icd_entry::NOT_IMPLEMENTED`] is the machine-readable list.

// `VkPhysicalDeviceLimits` has 110 members and `VkPhysicalDeviceProperties` is
// 800 bytes. Building those with a struct literal means naming every member,
// including the ~70 whose correct value is zero, which turns a reviewable list
// of the limits we actually claim into a wall nobody reads. `Default` then
// assignment says exactly which numbers are ours.
#![allow(clippy::field_reassign_with_default)]

pub mod advertise;
pub mod arena;
pub mod icd;
pub mod present;
pub mod state;
pub mod transport;
pub mod wsi;

/// The identity this build puts in the handshake, and checks on attach.
///
/// Must equal `sv_gpu_server::protocol_ids()`. Both are `sv_proto`'s hash over
/// the generated command and struct table, which is what makes a version skew a
/// refusal rather than a desynchronisation (design.md §4.3, R13).
pub fn protocol_ids() -> sv_transport::control::ProtocolIds {
    sv_transport::control::ProtocolIds::new(WIRE_VERSION, sv_proto::PROTOCOL_HASH)
}

/// See `sv_gpu_server::WIRE_VERSION`.
pub const WIRE_VERSION: u32 = 1;

/// The environment variable that points at a GPU side.
///
/// An ICD cannot be given arguments — the loader `dlopen`s it — so the socket
/// has to arrive as environment. Unset means "no GPU side", which is a supported
/// configuration and not an error: the driver still loads and still enumerates.
///
/// On Windows the value is a **session name** rather than a path: there is no
/// Unix socket, and the pipe and the two section objects are all named from one
/// string ([`sv_transport::winpipe::pipe_name`],
/// [`sv_transport::winshm::section_name`]). One variable for both platforms is
/// deliberate — a second variable for the same job is how somebody sets the one
/// that is not read on the OS they are on and gets a driver that answers
/// everything locally without saying so.
pub const SOCKET_ENV: &str = "SV_SOCKET";

use core::ffi::{c_char, c_void};
use std::sync::OnceLock;
use sv_icd_entry::{PFN_vkVoidFunction, VkInstance, VkResult};

/// The highest loader interface version we implement (design.md §8).
///
/// 7 relaxes the export requirements — an ICD at 7 need only export the three
/// `vk_icd*` symbols. 4 is where `vk_icdGetPhysicalDeviceProcAddr` becomes
/// required for unknown physical-device entry points. At ≤ 4 the *driver* must
/// validate `apiVersion` itself, which is the reason to be prepared for 5 rather
/// than to assume 7.
pub const MAX_LOADER_INTERFACE_VERSION: u32 = 7;

/// The version the loader and we settled on.
///
/// Recorded rather than discarded because the ICD's obligations differ by
/// version, and a bug report that says "which interface version did it
/// negotiate" should have an answer.
static NEGOTIATED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn negotiated_interface_version() -> u32 {
    NEGOTIATED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The one implementation, created on first use.
///
/// Created lazily rather than in a library constructor: an ICD is `dlopen`ed by
/// the loader during `vkCreateInstance`, and doing work — in our case creating a
/// memfd — in an ELF constructor means doing it in every process that so much as
/// enumerates drivers.
static ICD: OnceLock<Option<&'static state::SharedVulkan>> = OnceLock::new();

fn instance() -> Option<&'static state::SharedVulkan> {
    *ICD.get_or_init(|| {
        // A GPU side, if one was named. Failing to reach it is fatal rather
        // than a silent fallback to the standalone driver: an application told
        // to use a GPU side and quietly given a driver that answers everything
        // from a table would produce plausible nonsense, which is the one
        // failure mode this project must not have.
        #[cfg(any(target_os = "linux", windows))]
        if let Some(sock) = std::env::var_os(SOCKET_ENV) {
            let sock = sock.to_string_lossy().into_owned();
            return match state::SharedVulkan::connected(&sock) {
                Ok(icd) => {
                    let leaked: &'static state::SharedVulkan = Box::leak(Box::new(icd));
                    sv_icd_entry::set_icd(leaked);
                    // The hash goes on the line that always prints, not behind
                    // `SV_DEBUG`. `Control::attach` already refuses a mismatch
                    // loudly and by name (`ProtocolHashMismatch { ours,
                    // theirs }`), so the failure is never silent — but it is
                    // read as a *transport* fault, and it never is one: it means
                    // one of the two binaries is older than the other. Printing
                    // ours beside the socket puts the number in every run's log,
                    // so the first question after a handshake failure is "are
                    // these the same build" rather than "is the ring broken".
                    // This project has lost time to a stale binary twice.
                    eprintln!(
                        "sv-icd: connected to the GPU side at {sock} (protocol {:#018x})",
                        sv_proto::PROTOCOL_HASH
                    );
                    Some(leaked)
                }
                Err(e) => {
                    eprintln!(
                        "sv-icd: {SOCKET_ENV}={sock} but the GPU side is unreachable: {e}\n\
                         sv-icd: refusing to load rather than answering locally"
                    );
                    None
                }
            };
        }

        let arena_len = std::env::var("SV_ARENA_BYTES")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(crate::arena::round_up)
            .unwrap_or(state::DEFAULT_ARENA_LEN);
        match state::SharedVulkan::new(arena_len) {
            Ok(icd) => {
                let leaked: &'static state::SharedVulkan = Box::leak(Box::new(icd));
                sv_icd_entry::set_icd(leaked);
                Some(leaked)
            }
            Err(e) => {
                // A driver that cannot map its arena is a driver that cannot do
                // anything useful, and saying so once is better than failing
                // every call with a different error.
                eprintln!("sv-icd: cannot create the arena ({e}); the driver will not load");
                None
            }
        }
    })
}

/// Make sure the implementation is registered before any entry point runs.
///
/// Every path into this driver goes through one of the three exports below, so
/// this is the complete set of places it has to be called.
fn ensure_ready() -> bool {
    instance().is_some()
}

// ---------------------------------------------------------------------------
// the three exports (design.md §8)
// ---------------------------------------------------------------------------

/// `vk_icdNegotiateLoaderICDInterfaceVersion`.
///
/// The protocol: the loader writes the highest version *it* supports, we lower
/// it to the highest *we* support, and both sides then behave as that version.
/// An ICD that does not export this symbol is assumed to be at version 0, which
/// loses `vk_icdGetPhysicalDeviceProcAddr` and the relaxed export rules.
///
/// # Safety
/// `p_version` is a pointer the loader provides, valid for one `uint32_t`.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn vk_icdNegotiateLoaderICDInterfaceVersion(
    p_version: *mut u32,
) -> VkResult {
    if p_version.is_null() {
        return VkResult::VK_ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: the loader's contract for this call.
    unsafe {
        let loader = *p_version;
        let agreed = loader.min(MAX_LOADER_INTERFACE_VERSION);
        *p_version = agreed;
        NEGOTIATED.store(agreed, std::sync::atomic::Ordering::Relaxed);
    }
    VkResult::VK_SUCCESS
}

/// `vk_icdGetInstanceProcAddr`.
///
/// # Safety
/// `instance` is null or a handle this driver returned; `p_name` is a
/// NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn vk_icdGetInstanceProcAddr(
    instance: VkInstance,
    p_name: *const c_char,
) -> PFN_vkVoidFunction {
    if !ensure_ready() {
        return None;
    }
    // SAFETY: forwarded unchanged; the contracts are the same.
    unsafe { sv_icd_entry::local::sv_vk_get_instance_proc_addr(instance, p_name) }
}

/// `vk_icdGetPhysicalDeviceProcAddr`.
///
/// Required from loader interface 4, and it answers only for commands whose
/// first parameter is a `VkPhysicalDevice`. The loader asks it about entry
/// points it does not itself know; returning something for a command it *does*
/// know is harmless, and returning something for a non-physical-device command
/// is not — the loader would call it with the wrong first argument.
///
/// # Safety
/// As [`vk_icdGetInstanceProcAddr`].
#[unsafe(no_mangle)]
pub unsafe extern "system" fn vk_icdGetPhysicalDeviceProcAddr(
    _instance: VkInstance,
    p_name: *const c_char,
) -> PFN_vkVoidFunction {
    if !ensure_ready() {
        return None;
    }
    // SAFETY: the loader passes a NUL-terminated string.
    let name = unsafe { sv_icd_entry::proc_name(p_name) }?;
    sv_icd_entry::as_pfn(sv_icd_entry::lookup(sv_icd_entry::PHYSICAL_DEVICE_COMMANDS, name))
}

/// `vkGetInstanceProcAddr`, exported as well as the `vk_icd*` three.
///
/// design.md §8 says "exactly" the three `vk_icd*` symbols, which is right at
/// loader interface 7. It is **not** right below it: at interface versions 0 and
/// 1 the loader looks for this symbol by name and finds nothing, and at 2–6 some
/// loaders still fall back to it. Exporting it costs one forwarding function and
/// removes a whole class of "the loader silently skipped the driver". The
/// deviation from §8 is deliberate and this is the argument for it.
///
/// # Safety
/// As [`vk_icdGetInstanceProcAddr`].
#[unsafe(no_mangle)]
pub unsafe extern "system" fn vkGetInstanceProcAddr(
    instance: VkInstance,
    p_name: *const c_char,
) -> PFN_vkVoidFunction {
    // SAFETY: forwarded unchanged.
    unsafe { vk_icdGetInstanceProcAddr(instance, p_name) }
}

/// The path of the manifest this build wrote, for tests and for a person who
/// wants to set `VK_DRIVER_FILES`.
pub const MANIFEST_PATH: &str = env!("SV_ICD_MANIFEST");

/// Keep `c_void` referenced in a way that survives a `--release` build without
/// the import being flagged unused on every platform.
#[doc(hidden)]
pub fn _abi_anchor() -> *const c_void {
    core::ptr::null()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_lowers_to_what_we_support_and_never_raises() {
        for offered in [0u32, 1, 4, 5, 6, 7, 8, 99] {
            let mut v = offered;
            // SAFETY: `v` is a live local.
            let r = unsafe { vk_icdNegotiateLoaderICDInterfaceVersion(&mut v) };
            assert_eq!(r, VkResult::VK_SUCCESS);
            assert_eq!(v, offered.min(MAX_LOADER_INTERFACE_VERSION));
            assert!(v <= offered, "an ICD must never negotiate upwards");
        }
        // A null pointer is a loader bug, and it must not be a fault in us.
        // SAFETY: passing null is the case under test.
        assert_eq!(
            unsafe { vk_icdNegotiateLoaderICDInterfaceVersion(core::ptr::null_mut()) },
            VkResult::VK_ERROR_INITIALIZATION_FAILED
        );
    }

    /// Miri's isolation has no filesystem, and the manifest is a fact about the
    /// build rather than about the code, so there is nothing here for it to
    /// check.
    #[cfg_attr(miri, ignore)]
    #[test]
    fn the_manifest_exists_and_points_at_this_build() {
        let text = std::fs::read_to_string(MANIFEST_PATH).expect("the build wrote a manifest");
        assert!(text.contains("\"file_format_version\""));
        assert!(text.contains("\"api_version\": \"1.3.0\""), "1.0 would demote us silently");
        assert!(
            text.contains("\"is_portability_driver\": false"),
            "true would make us invisible to almost every application"
        );
        assert!(text.contains("libsv_icd.so") || text.contains("sv_icd.dll"));
    }
}
