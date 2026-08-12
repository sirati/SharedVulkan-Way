//! `dmabuf-probe` — can a **Linux GPU side** import the arena at all?
//!
//! # The question, and why the reverse direction forces it
//!
//! design.md's topology is direction-agnostic on paper (§1): either side may be
//! the VM. In practice the two directions do not have the same import problem,
//! and the asymmetry is not in our code but in the drivers.
//!
//! - **GPU side inside the VM** (testhost). The arena is guest RAM; the guest
//!   allocates it, the host maps QEMU's memfd. The importing driver is Intel's
//!   Windows one, and it has been *measured* accepting a plain host pointer to
//!   exactly those pages, `memoryTypeBits = 0x6`, up to 2 GiB in one
//!   `VkDeviceMemory` (`measured-gpu-side.md`). Nothing more is needed.
//! - **GPU side on the host** (the conventional direction, a guest borrowing the
//!   host's GPUs). The arena still has to be guest RAM, because an fd cannot
//!   cross into the guest and guest RAM is the only memory both sides can map.
//!   But now the importing driver is NVIDIA or RADV, and both have been measured
//!   **refusing** a `MAP_SHARED` host pointer: RADV `INVALID_EXTERNAL_HANDLE`,
//!   NVIDIA `OUT_OF_DEVICE_MEMORY` (`measured-gpu-side.md`, "two drivers refuse
//!   a shared mapping").
//!
//! So the reverse direction needs a third mechanism, and `sv_arena::import`
//! already names the candidate: wrap the memfd as a **dma-buf** with
//! `/dev/udmabuf` and import *that*. udmabuf exists for QEMU and virtio-gpu,
//! which is a good sign it is the intended answer rather than a hack.
//!
//! Three things have to be true and none of them is documented anywhere as a
//! single fact, so each is a step here with its own verdict:
//!
//! 1. `/dev/udmabuf` is **openable** at all. On this machine it is `root:kvm`
//!    0660 and the developer account is not in `kvm` — the same shape of
//!    problem as `SeLockMemoryPrivilege` on the Windows side (R25).
//! 2. `UDMABUF_CREATE` accepts a sealed memfd and returns a dma-buf.
//! 3. **The driver imports it.** RADV advertises `VK_EXT_external_memory_dma_buf`
//!    and is expected to. Whether the NVIDIA proprietary driver accepts a
//!    dma-buf *it did not create* is the open question, and it is the one that
//!    decides whether the 5090 can serve a guest at all.
//!
//! Step 3 is the reading. Steps 1 and 2 are prerequisites, and the probe reports
//! which of them stopped it rather than a single failure.
//!
//! # The contrast rows, which are the point of running all of them
//!
//! For each device the probe tries, in order, and reports every one:
//!
//! | row | what it is | expected |
//! |---|---|---|
//! | `host-ptr(anon)` | ordinary anonymous memory, `VK_EXT_external_memory_host` | works on all three |
//! | `host-ptr(memfd)` | `MAP_SHARED` of a memfd — **the arena's real shape** | refused by NVIDIA and RADV |
//! | `fd(memfd-as-dmabuf)` | the memfd handed straight to `VkImportMemoryFdInfoKHR` | refused: a memfd is not a dma-buf |
//! | `fd(udmabuf)` | the memfd wrapped by `/dev/udmabuf` | **the reading** |
//!
//! A probe that only ran the last row could not tell "udmabuf does not work"
//! from "this device imports nothing", and the third row is there because
//! "just pass the memfd" is the mistake somebody will otherwise make: it is one
//! word different in the source and it fails with a plausible error.
//!
//! # Coherency is checked in both directions, or not at all
//!
//! Where an import succeeds and the memory is host-visible, the probe writes a
//! pattern through the memfd mapping and reads it through `vkMapMemory`, then
//! writes a second pattern back through `vkMapMemory` and reads it through the
//! memfd. A one-directional check passes against a driver that quietly handed
//! back a bounce buffer, which is the failure this whole project must not ship.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("dmabuf-probe: Linux-only — udmabuf and dma-buf are Linux mechanisms");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod probe;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    probe::main()
}
