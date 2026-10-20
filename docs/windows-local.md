# Windows can run both halves locally

A Windows machine can host the Wayland compositor and run native Windows
Wayland clients against it. A VM is optional. Local pixels use named Windows
sections, and the local Wayland connection uses AF_UNIX. No PCI shared-memory
device or VM driver is involved in this topology.

The current native client is `wc-shmclient.exe`, cross-built with MinGW. It
creates an xdg-shell toplevel, shares two buffers, and observes buffer releases
and frame callbacks. This exercises the existing Windows descriptor encoding.
It does not prove that an unmodified Cygwin libwayland application interoperates
with that encoding. Cygwin and a portable MinGW client library remain separate
compatibility work. The application must not be patched to hide that gap.

Build both executables on vmhost through the shared build wrapper:

```sh
/mnt/datasets/builds/wcrun/cg nix develop -c cargo build \
  -p wc-compositor -p wc-shmclient --target x86_64-pc-windows-gnu
```

Run an isolated Wine check from the dev shell. It allocates its own X display,
uses a fresh Wine prefix and HOME/XDG directories, and kills only processes
carrying its unique run marker through pinned Linux process descriptors.
Evidence must be outside the repository and the directory must be new.

```sh
python3 scripts/wc-windows-local.py \
  --evidence /mnt/datasets/builds/evidence/windows-local/UNIQUE
```

A pass establishes that the native Windows server and native MinGW client
exchange and present thirty frames using local named sections, then release
the window and client before a clean compositor exit. It is a Wine result until
run on real Windows. It does not measure VM latency or establish Cygwin support.
The script also runs directly with Windows Python when given `--bin-dir` and
an evidence directory. Use a short writable `--work-root` if the default
temporary path exceeds the Windows AF_UNIX limit. This test opens a temporary
local window, so run it only on an authorized desktop. It does not deploy binaries or alter system settings.

Vulkan client/server roles are independent of these Wayland roles. Windows
already has a same-machine ICD attachment over named sections and a named
pipe. A Windows or macOS VM acting as a Vulkan client needs an appropriate
shared-memory device mapping and doorbell path. The current Windows ICD
connects through a local named pipe and opens named control and arena sections.
It does not attach a PCI shared-memory device or run a cross-VM doorbell
receiver. Windows Vulkan clients across a VM boundary therefore still need
ICD attachment and wake integration, even where a device driver already exists.
A local Windows test does not prove that VM topology. macOS guest-driver work
is tracked separately.

The isolated Wine 11.14 run on 2026-10-20 passed those checks with the native
MinGW binaries. It created one HWND and destroyed it, presented all thirty
frames, reported zero remaining windows or clients, and completed scoped
process cleanup. This did not inspect screen pixels or exercise real Windows.
