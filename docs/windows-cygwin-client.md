# Cygwin and MinGW Wayland clients

The native MinGW `wc-shmclient` uses our Windows descriptor dialect. It is
not a libwayland compatibility test. A Cygwin executable also has a different
C ABI from MinGW: x86-64 Cygwin is LP64, while native Windows is LLP64.
Treating a Cygwin fd as a Winsock handle is incorrect on both the descriptor
and socket layers.

The official Cygwin package catalog sampled on 2026-10-20 contains no
libwayland or Weston package. `scripts/wc-cygwin-build.sh` cross-builds a
client-only upstream libwayland 1.24 and the unchanged upstream Weston 1.4
`weston-info` application using native clang/lld on Linux. The official
Cygwin runtime, CRT, FFI library and headers are pinned in
`integrations/cygwin/packages.json`; source hashes are separate in
`sources.json`. The extracted target GCC compiler is never executed.
Native lld links against its CRT objects because clang's Cygwin driver
otherwise delegates linking to a target GCC executable. Compile-time
assertions require the actual Cygwin LP64 ABI. The artifact manifest records
hashes and PE imports and verifies the application imports `cygwin1.dll`.

```sh
scripts/wc-cygwin-build.sh --scratch /mnt/datasets/builds/scratch/cygwin-client
```

This build alone proves neither Windows runtime compatibility nor rendered
Wayland applications. The library integration in `integrations/cygwin/wayland-os.c`
refuses outbound ancillary descriptors with ENOTSUP. Cygwin AF_LOCAL streams
silently drop ancillary data; the optional AF_UNIX backend's sendmsg/recvmsg
are unsupported in the audited source. Pretending SCM_RIGHTS arrived would
turn the first wl_shm pool into a misleading protocol failure.

The portable bundle includes a Cygwin ABI diagnostic and a descriptor probe.
The probe returns 0 only if a live descriptor really transfers, and 2 if
that facility is absent. A missing descriptor is recorded as a limitation,
not a passing buffer test. `weston-info` is a real, unmodified libwayland
application, but it only queries globals and does not map a window.

The separate `wc-cygwin-link` native DLL and in-process libwayland adapter support
one descriptor-free registry connection across a VM boundary. The library
preserves Cygwin's local socketpair and POSIX readiness for the application,
with no fork or target compiler. With `WC_WAYLAND_RING=1`, libwayland selects
the ring adapter while the unchanged application still calls wl_display_connect. The adapter's
native DLL alone requests the Wayland BAR and doorbell from the privileged
Windows broker. Every control/ring view retains the actual driver mapping
owner. The Linux `wc-cygwin-host` connects the ring to a Linux Wayland socket,
uses an eventfd doorbell wake without a periodic idle poll, and refuses any
compositor SCM_RIGHTS event. It never creates, resizes or truncates the
QEMU BAR backing object. The object must already be at least 6 MiB.

Build the native bridge through the shared build wrapper, then include it
in the Cygwin bundle:

```sh
/mnt/datasets/builds/wcrun/cg nix develop -c cargo build \
  -p wc-cygwin-link --target x86_64-pc-windows-gnu
/mnt/datasets/builds/wcrun/cg nix develop -c cargo build \
  -p wc-cygwin-link --bin wc-cygwin-host
scripts/wc-cygwin-build.sh --scratch /mnt/datasets/builds/scratch/cygwin-client \
  --bridge-dir target/x86_64-pc-windows-gnu/debug
```

The runtime fixture must be coordinated with the Windows Vulkan client
owner. Use exactly one copied disposable Windows VM, a separate Wayland
BAR at slot 0x1c, and `org.sirati.wcbell.0`. Its Vulkan BAR at 0x1d belongs to
the other transport. Never detach the daily VM's iGPU or assume a ghost PnP
device is present. Keep profiles and HOME/XDG state under a throwaway run
root. A same-machine MinGW test or a registry roundtrip cannot establish a
rendered Cygwin window, a zero-copy wl_shm path, or hardware Vulkan results.

Rendered Cygwin clients still require a BAR-backed application mmap/fd
adapter and descriptor translation at the Linux compositor boundary.
Ordinary file-backed Cygwin mmap followed by copying pixels into a BAR is
not an accepted substitute. GPU forwarding remains upstream Mesa/Zink and
is owned by the Windows Vulkan client work.


Real Windows checks on 2026-10-20 used the shared disposable unaccelerated
clone with 3 GiB and four vCPUs. The cross-built Cygwin LP64 executable ran
successfully. Its real socketpair probe sent and received the byte but
received zero descriptors, confirming that SCM_RIGHTS is unavailable.
The separate native MinGW client/server check committed and presented all
30 frames using two named-section buffers and released its HWND/client
cleanly. These were runtime checks on Windows, not Wine. No screenshot or
hardware-rendering claim is made by that same-kernel GDI check.

The reverse registry runtime also passed on real Windows with the same clone.
Unchanged Cygwin `weston-info` enumerated the Linux compositor globals through
broker-lent BAR rings and the doorbell. The host recorded 136 request bytes,
1,008 reply bytes and zero descriptors. The Linux compositor was headless;
this proves registry/roundtrip compatibility only. The clone stopped normally
after the host writer exited. Real Cygwin rendered buffers remain unsupported.
