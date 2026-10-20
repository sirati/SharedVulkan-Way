# Windows Vulkan client across shared memory

`SV_SOCKET=ivshmem:0x1d` selects the Windows guest client path. The GPU server
runs on the Linux host. The Windows client does not need GPU passthrough.
Other `SV_SOCKET` values retain the separate same-machine named-pipe path.

The Linux owner runs the existing shared-pool publisher through the explicit
host entry point:

```
sv-gpu-server pool-host --pool-file /path/to/qemu-gpu-bar \
  --bell socket:/path/to/qemu-svbell.sock --sessions 1
```

The file must already exist at its configured size and back QEMU's GPU
ivshmem BAR. This mode never resizes it. It rejects `--grow-ram` and the debug
TCP `--port`. Importing its pages requires actual
`VK_EXT_external_memory_host` support from the selected host Vulkan driver.
A successful software-driver protocol test does not establish hardware import
support. In particular, earlier RADV measurements refused this import.

The Windows broker must explicitly bind the actual present GPU device's PnP
instance to the client's reviewed SID. Configure its GPU device at PCI slot
0x1d and its `svbell` virtio-serial port. The application sets:

```
SV_SOCKET=ivshmem:0x1d
SV_BELL=org.sirati.svbell.0
WC_BROKER=\\.\pipe\waycompose-broker
```

Use the matching built `sv_icd.dll` and manifest with the native Windows Vulkan
loader. The application requests broker role `vulkan-client`. It never falls
back to opening a raw device if the broker refuses. Whole-BAR access is within
the SID's trust domain; individual sessions do not isolate a hostile process
of that same SID from the rest of the BAR.

The existing pool cold exchange works in this direction without a new wire
format. The GPU owner publishes Hello offers; the Windows claimant validates
protocol IDs, the pool nonce, geometry, aligned bounds and every control-page
stamp before Accept. After PUBLISHED, it checks the nonce again and verifies
that the published Control geometry and generation match the offer.

Each safe region slice retains its parent and the original mapped-device
owner. One process registry shares the BAR mapping, mailbox liveness monitor
and doorbell reader between its sessions. The per-range doorbell dispatches
cold wakes and every live Control through a ControlSet. An absent bell uses
bounded polling. A specified but refused bell fails attachment.

This path supports the original BAR region only. It rejects a pool advertising
capacity beyond that BAR. A lease for another region fails instead of mapping
unrelated memory. The Windows ivshmem driver permits one mapping per device;
independent application processes may therefore receive a mapping refusal.
A guest mapping proxy for several processes is separate work.

`vk-probe --fill-buffer` provides a bounded real workload. It initializes a
256 KiB mapped buffer to zero, submits a GPU fill, applies a transfer-to-host
barrier, waits at most ten seconds for the fence, and verifies all 65,536
words. This exercises the client ICD and remote server without a WSI surface.
It does not establish Win32 presentation support.

Offline validation passed at `d29df89`: 157 transport tests, six ICD transport
regressions, Linux server check/release build, Windows ICD check/release build,
strict transport lint and Linux/Windows ICD library lint, plus the Windows
probe build/lint. Existing ICD all-target lint hits an unrelated
`state.rs` items-after-test-module warning. Runtime results must be reported
separately from these compile/protocol checks.

The real reverse-topology Vulkan leg passed on 2026-10-20 at executable
`64a14b0`. The GPU server ran natively on vmhost's NVIDIA GeForce RTX 5090;
the client ran inside disposable Windows `wcram` with 3 GiB RAM, four vCPUs,
no GPU passthrough and a 64 MiB shared BAR. The native Windows Vulkan loader
loaded the MinGW ICD, obtained the BAR and virtio-serial port from the SID-bound
broker, attached through the pool mailboxes and submitted the fill workload.
The Linux server reported the NVIDIA device and leased host memory type 2.
All 65,536 words contained the GPU fill pattern. The watchdog stayed healthy;
the Linux writer exited before the VM stopped normally.

This proves that hardware/driver and role direction. It does not prove
testhost's Intel iGPU topology: read-only inspection found its only display
function, 0000:00:02.0, bound to vfio-pci while the daily Windows VM was active.
No device was rebound and that VM was not restarted. The separate Cygwin
Wayland-client proof remains owned by its adapter work; Vulkan success is not
a substitute for Cygwin compatibility or presentation.
