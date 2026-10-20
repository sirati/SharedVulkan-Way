# Native MinGW Wayland client through the shared BAR

The native Windows transport's explicit `broker-bar` endpoint selects the
broker-lent Wayland BAR and doorbell. `WC_WAYLAND_RING=1` also selects this
connection for a library client using its ordinary display-name path. Other
socket paths without that explicit setting retain their existing
same-machine connection behavior. The socket facade carries Wayland wire
bytes over the shared ring; its socketpair pumps never carry pool pixels.

The transport selects a common section provider before a client allocates
its first pool. `Handle::create_section` then allocates the original backing
from the same BAR ownership directory used by the Cygwin adapter. A provider
failure ends allocation; it never falls back to a guest-local section. The
mapping retains the BAR owner, while common producer, pending-transfer and
consumer references coordinate each extent's normal lifetime. This directory
is cooperative state, not a privileged allocation authority. Normal explicit
Handle/map release can be measured; hostile peers and abrupt process death
need the separate protected driver/broker retirement work. Abandoned mappings
are retained rather than treating DLL teardown or elapsed time as a drain.

Native applications must send resources through
`wc_transport::send_handle_message`. That API prepares one resource transfer,
builds the complete protocol frame, and commits after a successful write.
A failure before any frame byte is emitted rolls the reservation back. A
partially emitted failed frame closes the connection and conservatively
retains the pending reference, since the receiver may already have accepted
the descriptor. `Descriptor::encode` remains a pure codec and does not
transfer ownership by itself.

The project's `wc-shmclient` fixture uses this shipped API. Its painter and
Wayland protocol implementation remain the same; its pool emission now uses
the common resource-transfer boundary. It waits for the final frame callback
before reporting completion and records the final pixels' hash, allowing a
Linux compositor's presented hash to establish that those pixels arrived.
After releasing its Handle and mapping, it calls
`wc_transport::finish_bar_connection` synchronously. Native process exit does
not join socket pump threads, so dropping a socket just before exit is not a
ring teardown acknowledgement. This explicit finish closes the normal
connection; it does not retire abandoned allocations.

This native fixture is our own client. It is separate from the unchanged
upstream Cygwin Weston client evidence. It does not establish general native
MinGW libwayland compatibility or Win32 Vulkan presentation support. An ordinary
application must use a compatible native Wayland library and original backing
allocator; editing an upstream application to copy its buffers into the BAR
would not meet this contract.

Runtime prerequisites are the single disposable `wcram` fixture's separate
Wayland BAR at slot 0x1c and GPU BAR at 0x1d, the protected broker SID bindings,
and a Linux compositor attached to the authenticated pixel subwindow. The
native client uses the same pinned run and arena identity as the Cygwin route.
No raw device-open fallback or file-arena bypass is enabled. The directory
layout uses header format 3 and control wire version 2. Legacy format-2
clients must reject this header before touching pixel or allocation metadata;
new lease clients also reject legacy headers and control versions.

The native runtime passed on the disposable Windows guest with 3 GiB and four
vCPUs, without GPU passthrough. The fixture committed 30 frames, and the Linux
headless compositor presented 30 distinct pixel hashes. The final submitted
hash `6cd1c180a52e02a5` matched a Linux presentation. The wire carried 2,296
request bytes and 1,860 reply bytes while the original BAR held the two
400 by 300 pixel buffers. Normal explicit release left zero allocation
capacity, owner, pending-transfer and consumer counts; the host observed
`ring_dead=1` before the VM stopped normally.

A separate native Windows Vulkan probe in the same boot verified 65,536 words
from a 256 KiB GPU fill served by the Linux NVIDIA GeForce RTX 5090. This
establishes that separate Vulkan path. The Wayland pixel fixture uses wl_shm;
it does not establish Vulkan presentation or the requested laptop iGPU path.
The runtime evidence is `windows-mingw-client/real-windows-5gOkPt`, with exact
binary hashes, logs, directory snapshot and QEMU arguments. Scoped validation
passed 115 tests, strict Linux and Windows transport/client lint, and the
native client plus Linux host and compositor builds.
