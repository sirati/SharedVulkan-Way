# Linux clients on a guest PCI BAR

`sv-hostd ivshmem-serve` can attach a Linux app-side guest to a GPU pool
owned by its VM host. The privileged broker opens the configured PCI BAR
and virtio-serial port. Hostd borrows both descriptors and retains its broker
connection for the device loan. Desktop processes do not open raw resources.

```
sv-hostd ivshmem-serve --region-device gpu-bar --region-bytes 67108864 \
  --cold pool --bell-device svbell --bell-hub /run/app/bell.sock \
  --socket /run/app/gpu.sock --drm-services /run/render/services.sock
```

The broker must have exact, root-configured device paths. Its socket must be
restricted to the fixture's single authorized UID. This preparation supports
one UID for the whole backing lifetime, including surviving client mappings.
It does not provide durable reassignment of a BAR to another UID after a
broker restart. Do not deploy it for a multiuser system without that policy.

Hostd maps the original BAR descriptor. It checks the advertised size against
the configured bound and validates protocol, nonce, geometry, range and every
control-page stamp through `sv_transport::poolattach`. It reads the published
control magic from the mapping, since a PCI resource supports mmap and need
not implement ordinary file reads. No BAR bytes are copied to a file or memfd.

Only descriptor attachment is accepted on this path. The render-node relay
provides sandboxed clients with the actual BAR descriptor, including region
zero requests after the initial handshake. Additional regions are refused;
this path has no dynamic-region publication yet. Vulkan allocations inside
the initial pool retain the common allocator's dynamic lease behavior.

The broker-lent virtio-serial file uses `PortHandle::split` and the existing
bounded shared doorbell hub. Reopen rescan and bounded writer queues remain
in effect. The hub registers the same per-UID render-node service as the
normal socket-backed hub. Only wake bytes use the port. Commands, pool
offers, replies and GPU data stay in the original BAR mappings.

The isolated Linux fixture uses a cached NixOS kernel, three GiB of RAM,
two vCPUs, no network or physical-device passthrough, and a ten-minute bound.
A real root CUSE endpoint and unchanged Firefox default-sandbox software run
were observed. These alone do not prove driver initialization, GPU rendering
or hardware video decode. Those require the separate BAR/fill and RDD gates.

The bounded kernel fixture then passed `vk-probe --fill-buffer` against an
RTX 5090 on the VM host. The guest verified all 65,536 words in 256 KiB
after a transfer-to-host barrier and fence. An independent read of QEMU's
original shared backing found the complete contiguous GPU-written pattern.
Hostd mapped the root-owned PCI resource, whose direct open by the desktop
UID was denied. The app could not connect to the GPU socket path and
attached through the real CUSE relay by descriptor. The actual virtio-serial
doorbell was attached. The 26 ring waits averaged 1.753 ms on that run,
which does not meet the 200 us latency target.

These gates passed: 49 hostd and 158 transport unit tests, including the
broker-lent file-port hub test, and native builds of hostd, broker, ICD,
GPU server and probe. Five stopwatch tests were ignored. Clippy completed
with existing warnings outside the changed lines. Strict warning denial
stopped on pre-existing render-node warnings.

## Bounded native-import diagnostics

`scripts/sv-linux-bar-audit.py prepare` freezes a clean, explicitly selected
source commit, the completed build log and every actual component hash into
an artifact directory outside git. The component source bindings are
explicit because the compatible GPU owner, hostd and tracing ICD may have
different source commits. The copied executable artifacts avoid mutable
worktree targets. The receipt records package manifests and resolved ELF
paths. `verify --receipt FILE` refuses changed bytes before a launch.
Build-log/source correspondence still requires review; a matching file hash
alone does not prove how an executable was built.

Required named artifacts are `sv-drm`, `sv-hostd`, `wc-broker`,
`sv-gpu-server`, `vk-probe`, `qemu`, `kernel`, `initrd`, `boot-script`,
`setup-script`, `gpu-script` and `guest-runner`. Supply the actual final
Firefox payload, tracer and build log through their separate options.
The final payload must select immutable Firefox, Weston, Mesa, ICD,
geckodriver and jq packages. The six binary component source bindings also
include `icd_library`. Use the traced ICD's reviewed immutable package,
not an earlier package with the same wire protocol.

The derived payload changes only the compositor invocation to an audit
wrapper. Only Weston receives `WAYLAND_DEBUG=server`, `SV_DMABUF_TRACE=1`
and syscall tracing. Firefox and its default sandbox remain untraced.
The wrapper keeps the original compositor arguments, uses one bounded
64 MiB trace file and terminates its own compositor process group during
cleanup. This diagnostic run cannot establish latency results.

Protocol logging records the actual linux-dmabuf create request, format,
modifier, plane indices, received descriptors, offsets and strides. Native
tracing preserves socket identities, SCM_RIGHTS descriptors and actual
ioctl/mmap errors before a Vulkan import. The reviewed ICD's opt-in logs
separately identify format-query, image-create and import entry/results.
If those entries are absent, do not invent an ICD error for a rejection in
Mesa, EGL or GBM. Preserve the entire logs, process maps/status and crash
artifacts, including `.dmp` and `.extra` files, outside git.

`decode --protocol weston.log --trace native-import.strace --output FILE`
writes a diagnostic JSON receipt. It preserves raw native failure lines and
refuses to mark missing planes, malformed fields or observed object-ID
collisions complete. Wayland object IDs are client-local, so correlate the
protocol lines with native socket/descriptor records before assigning a
buffer to an importer. The parser is not an import-success verdict.
The focused parser checks cover multi-plane modifiers, repeated client-local
IDs and incomplete records. No GPU or VM starts during prepare, verify,
decode or those checks.

Every next runtime must independently retain the initial original-backing
fill check and actual BAR-size/permission/mapping receipts, then record the
exact executed binaries and arguments against the frozen snapshot. Stop all
guest clients and services, then the host GPU writer, then the disposable VM.
Assert both scopes are inactive before removing the owned disk and backing.
The three GiB, two vCPU, no-network, 600-second VM and 480-second GPU bounds
remain. Increasing the initial test BAR to 256 MiB is fixture provisioning;
it does not implement dynamic region growth or change allocation reserves.
