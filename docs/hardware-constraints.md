# The machine this has to run on

Measured on the real target, 2026-08-11, not assumed. Everything below was
read off the running system rather than from documentation.

## Topology, and why it is backwards

`testhost` is a NixOS host whose entire job is to run a Windows 11 guest.
The machine has exactly one GPU — an Intel iGPU, Raptor Lake `8086:a7a8` —
and it is passed through to the guest with VFIO:

    00:02.0 VGA compatible controller: Intel Corporation Raptor Lake-P [UHD Graphics]
        Kernel driver in use: vfio-pci

So the Linux host has **no GPU at all**. The usual arrangement — host owns the
GPU, guest borrows it — is inverted here. Relative to the graphics device the
**VM is the server and the host is the client**.

This is the single most important constraint on the design, and the reason
existing solutions do not drop in: Venus, sharedgl, virgl and friends all
assume the guest is the one asking to borrow.

## QEMU

    QEMU emulator version 10.2.2

A pinned, locally patched build (ACPI battery passthrough), so we control what
it is compiled with and can rebuild it if a device needs enabling.

### Shared memory transports available

    ivshmem-plain      bus PCI, "Inter-VM shared memory"
    ivshmem-doorbell   bus PCI, "Inter-VM shared memory"

Both variants are present, so the ivshmem route is open without rebuilding
QEMU. `-plain` is a bare shared region; `-doorbell` adds interrupt signalling
via an ivshmem server, which is what makes an event-driven ring possible
rather than a spinning poll.

Memory backends available:

    memory-backend-file
    memory-backend-memfd
    memory-backend-shm

Note the guest already runs on `memory-backend-memfd` with `share=on`, because
virtio-fs requires it. Shared guest RAM is therefore already the norm here and
costs nothing extra under VFIO, where every guest page is pinned anyway.

### GPU-adjacent devices present

    virtio-gpu-pci / virtio-gpu-gl-pci
    virtio-gpu-rutabaga-pci
    vhost-user-gpu-pci

`virtio-gpu-rutabaga` is worth attention: rutabaga_gfx carries **gfxstream**,
Android's Vulkan/GLES forwarding implementation, which is the most mature
"serialise Vulkan across a boundary" codebase in existence. It is prior art
that must be evaluated before writing our own protocol, even though its
direction assumption is likely the conventional one.

## Devices already on this guest

Relevant because they are transports we could use without adding hardware:

- `virtio-serial-pci,id=vioserial` — carries the QEMU guest agent, and now
  also `org.sirati.wifi.0`. Ports can be hot-added to a running guest.
- `vhost-user-fs-pci` (virtio-fs), tag `hostshare`.
- A host-only virtio-net link, `<host-nat-ip>` <-> `<guest-nat-ip>`, independent of
  the WiFi the guest's LAN address rides on.

None of these is a good fit for a graphics command stream — virtio-serial is a
byte pipe and the network link is a copy — but they are useful for control,
handshake and out-of-band signalling, and they already work.

## Guest side

Windows 11, virtio-win guest tools installed, so the guest already has virtio
drivers for net, serial, fs, balloon, rng and input. Anything we add that
speaks virtio has a driver story; anything that needs a *new* kernel-mode
driver on Windows has a signing problem, which is a hard constraint on this
machine — it is a family laptop in production, not a test bench, and
permanent test-signing mode is not acceptable.

ivshmem on Windows needs a driver. Check what exists (the VirtIO/ivshmem
Windows driver from the virtio-win project, or Red Hat's) and whether it is
signed, before committing to that transport.

## Constraints that are not negotiable

- The guest is in production use by a non-technical user. It cannot be
  restarted casually and cannot be left in a broken state.
- The host is reachable only over the network. Anything that risks the host's
  networking or its ability to run the guest is unacceptable.
- No permanent Windows test-signing mode.

## Host limits the design depends on

    vm.max_map_count = 1048576

Measured 2026-08-11. This is not trivia: the app side builds its view of the
arena by mapping the guest's pages out of QEMU's memfd, and those pages are not
contiguous in guest-physical space, so a contiguous host-side view costs one VMA
per discontiguous run. A 64 MiB slab of ordinary 4 KiB guest pages measured
~1471 runs (`measured-gpu-side.md`), so the 4 KiB path costs roughly 23 VMAs per
MiB of arena — about 24k for a 1 GiB arena.

At the kernel's older default of 65530 that would have capped the arena at
around 2.8 GiB, and at a size that varies with the *guest's* memory
fragmentation rather than with anything we control, which is the worst kind of
limit. At 1048576 it is a non-issue: a 1 GiB arena uses ~2% of the budget.

Two consequences worth stating rather than assuming:

- The design should **check this at start-up and refuse to run if it is too
  low**, rather than failing at an arbitrary `mmap` half way through attaching a
  slab. It is a one-line read of `/proc/sys/vm/max_map_count`.
- Large pages make it nearly free — the same 64 MiB measured 22 runs, and a
  1 GiB large-page window measured **1**. The VMA budget only binds the 4 KiB
  fallback path.
