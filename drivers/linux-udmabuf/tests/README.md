# Private kernel fixture

These are actual Linux kernel integration tests. They are for an isolated,
matching Linux 6.18.55 KVM guest only. They do not load anything on the host,
change the stock udmabuf node, or activate a product backend.

Build the exporter and synthetic importer through the shared build queue:

```sh
/mnt/datasets/builds/wcrun/cg nix develop -c env \
  UDMABUF_TEST_NODE=1 UDMABUF_BUILD_TEST_IMPORTER=1 \
  scripts/wc-udmabuf-protected-build.sh PREPARED_SOURCE KERNEL_BUILD OUTPUT
/mnt/datasets/builds/wcrun/cg nix develop -c cc -O2 -g -Wall -Wextra -Werror \
  -pthread drivers/linux-udmabuf/tests/alias-retirement.c -o OUTPUT/alias-retirement
```

Use a compiler matching the prepared kernel when building modules. Copy the
three resulting artifacts into the private guest, record their hashes and the
exact kernel/config identity, and use plain `insmod` there. Never use force-load
or modify signature/security policy. Run `alias-retirement` as the guest's root
fixture process. It opens only `/dev/udmabuf-protected-test` and the separate
`/dev/wc-udmabuf-import-test`. Unload both test modules normally afterwards and
verify that the original `/dev/udmabuf` device identity is unchanged. Preserve
kernel logs and shut the guest down cleanly.

The user fixture checks immutable RO mmap/mprotect/execute restrictions, range
bounds, twelve concurrent CPU-sync threads, duplicate descriptors, inherited
and split VMAs, queued and received SCM_RIGHTS, unprivileged RO recipients,
producer and recipient exit while independent aliases survive, and eventual
retirement after the last alias ends. Child recipients close and unmap the
inherited original source before dropping privilege or receiving an export.
They therefore do not retain the broker's privileged original backing view.

The importer fixture retains real dma-buf references, a device attachment, an
SG DMA mapping and a kernel vmap. It can add an unsignaled BOOKKEEP fence to the
buffer's reservation object and then drop all its buffer/attachment/map refs.
An independently retained fixture fence reference permits a later explicit
signal. The user fixture requires the monitor to remain DRAINING until that
signal and to become RETIRED afterwards. This exercises kernel reference and
fence machinery, without GPU hardware or DMA submission. It is not proof of
any specific GPU importer's reference/fence behavior.

The fixture driver has no payload allocator or authorization policy. Its
control node is root-only and its open file owns all fixture state. Closing
that file signals any outstanding synthetic fence and cleans up the native
references, so a failed assertion does not deliberately strand the guest.

Logs, binaries, modules, resource measurements and boot recipes belong outside
Git under `/mnt/datasets/builds/evidence/linux-udmabuf-retirement`. Product
activation and actual GPU/cross-VM completion remain separate gates.
