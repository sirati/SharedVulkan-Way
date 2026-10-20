# Building on Linux

The source patch applies to Hyper Linux
`8048a96ba0abc7e2f548da684690bf415c99214e`. The application factory was audited
against hyper-linux-examples `e2b7357e79abe7e1b4ba5090403f7e5e17c1cf7e`.
It replaces that factory's X11 route with the existing native compositor.

All native and guest code builds on the Linux workstation. Run build commands
through the project's shared `cg` wrapper. Never use a Darwin remote builder.
`runtime.nix` and `backend.nix` enforce Linux build/ARM64 Darwin host platforms;
`icd.nix` and `client-probe.nix` enforce Linux build/ARM64 Linux host platforms.
The SDK is an explicit local input and is not committed.

`runtime.nix` adapts an existing Linux-to-Darwin Hyper Linux derivation, preserves
its entitlement signing and links the native Rust static library. It does not
convert the upstream Darwin-only flake into a cross toolchain. The offline
`scripts/build-hyperlinux.sh` provides a direct cross-build using the project's
clang/lld dev-shell tools, an ARM64-target-enabled Linux Rust toolchain, a Linux
codesign-compatible tool and the local SDK. It extracts the upstream assembler
text blobs without relying on Linux GNU objcopy having Mach-O support.

`mk-app.nix` takes the cross-built native runtime, Bash, session-services package
and compositor ensure package. Its default guest ICD and upstream Mesa package
are built for ARM64 Linux. It returns `.app`, suitable for a flake `apps` output,
and `.launcher`, `.root`, `.closure` for explicit inspection and deployment.
`compositor.nix` produces the native user's compositor ensure package from the
combined native `sv-guest` binary and a separate Darwin Vulkan driver manifest.

The native session-services package is maintained in
`nix/macos-session-package.nix`. It provides
`wc-macos-session-services --ensure` and concrete D-Bus/PulseAudio/portal behavior.
App launchers neither kill those services nor stop another application's
compositor on exit.

The public outputs below assemble these inputs. Constructing that closure is
not a Darwin application runtime test. See
[the topology and acceptance gates](../../docs/hyperlinux.md).

`stack.nix` supplies complete Darwin application and headless-probe outputs in
the root flake. On vmhost, build `.#packages.aarch64-darwin.hyperlinux-firefox` or
`.#packages.aarch64-darwin.hyperlinux-shared-probe` with native remote builders
disabled. Copy the realized closure before running `nix run
.#hyperlinux-firefox` or `nix run .#hyperlinux-shared-probe` on an ARM64 Mac.
The shared probe runs the shared-page and render-node/seals fixtures without
launching the GUI or user services. Native KosmicKrisp
keeps its existing authorized MTLTexture import patch; Linux Mesa/Zink/VA remain
upstream. Native binaries are signed after their final modifications with the
Linux-native rcodesign tool, preserving hl's Hypervisor entitlement.
