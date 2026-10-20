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

Connected Unix stream sockets support `SO_PASSCRED`. Incoming Linux credential
messages contain only Darwin kernel peer UID, GID and native PID, queried before
receiving bytes. Guest-supplied credentials never define that identity. Shared
option storage follows the open description through dup, fork and descriptor
transfer between the same exact native hl executable. Internal option descriptors
are hidden from the guest and recognized only by protected inode metadata.
Sending such a socket through an arbitrary native peer fails with EOPNOTSUPP,
because that peer cannot preserve the shared option state for a later import.
Unix datagram credential semantics are explicitly unsupported. This is connected
stream support, not a claim of complete Linux per-message credential semantics.

External guest pointers use thread-local nested access scopes. The dispatcher
and raw-pointer syscall helpers hold a scope through native I/O completion,
including vectored and message I/O. Resolving a shared view pins its owner and
checks the requested read or write permission. An access denial cannot fall
back to the primary guest buffer. Copy helpers create shorter nested scopes
for callers outside syscall dispatch. A raw external pointer without a scope
is refused.

Unmapping removes guest mappings and Stage-2 access immediately. If a native
call still holds a pointer, its backing, PRIME owner, writable seal reservation
and IPA reservation survive until the final scope ends. Successful Stage-2
removal plus final pointer completion releases those resources. A failed
Stage-2 removal retains the existing VM-destruction requirement.

`shared-access-check.c` compiles the actual resolver, protection, unmap and
retirement code against HVF/page-table stubs. It deterministically pauses a
reader after pointer acquisition, unmaps from another thread, and verifies
that pointer completion precedes backing release and IPA reuse. It also tests
read-only and PROT_NONE denial on genuine native read-only pages.
`shared-access-client.c` exercises ordinary Linux scalar, vectored and message
syscalls against O_RDONLY, PROT_READ and PROT_NONE shared views. It must also
pass under the integrated runtime before native acceptance is claimed. Its
gettimeofday check explicitly invokes the syscall to avoid a vDSO write.

Guest metadata copies use `mach_vm_read_overwrite` directly into their existing
caller destination. Strings copy into their existing result buffer before
scanning it. These kernel operations report an invalid or truncated source or
destination as a copy failure, which syscall callers translate to EFAULT.
They add no frame payload staging. Native scalar, vectored and message I/O
continues to use the pinned mapping directly in the kernel.
`copy-fault-check.c` is a native Darwin regression for deterministic and
concurrent truncate during these production copy operations. The Linux ELF
`shared-access-client.c` also covers truncated output and pathname buffers.
External futex atomic fault isolation is a separate pending gate; kernel copies
alone do not make native CPU atomics on a truncated file safe.

The native GPU endpoint defaults to eight session slots, matching the existing
pool endpoint. `--sessions` accepts 1 through 64. A slot covers the admission
queue, handshake and active GPU ownership. Rejected peers receive the existing
cold-protocol refusal without starting a worker or allocating an arena. A session
whose GPU/backing ownership cannot safely drain retains its slot permanently and
reports the retained count, bounding that failure budget. Bell readers and their
writer registrations have the same finite connection budget. Closing a bell
releases its registration only when the reader worker actually ends. These limits
do not evict an established idle owner.

External futex words use public ARM64 native leaf load/store/CAS helpers.
Each futex syscall temporarily swaps only its calling thread's BAD_ACCESS
exception port. A private in-process worker accepts only the public
EXCEPTION_STATE packet shape and named guest-address instruction PCs. It
returns EFAULT by resuming the same leaf at its normal error return. It never
unwinds C or Rust frames. An unrelated fault PC returns KERN_FAILURE to the
untouched task/system exception chain. No process-wide signal handler or task
exception port changes are used.

A preexisting custom thread BAD_ACCESS handler causes immediate atomic
restoration and refusal before guest atomic access. Debugger/custom-thread
exception configurations therefore remain unsupported. Normal and error
returns restore the previous single-bit thread exception chain in one kernel
call and release the acquired send rights. Restoration failure is fatal.
`atomic-fault-check.c` tests real truncate races, normal atomic operations,
thread-right counts, prior-handler refusal/restoration, task-handler identity,
and unrelated-PC rejection in a disposable spawned child. Cross-linking alone
is not native proof. The integrated Linux ELF acceptance also exercises
truncated futex words; its separate `--guest-map-fault` mode must print a guest
signal marker for a direct CPU load after truncate. That mode distinguishes a
delivered guest fault from a native host crash.
