# The goal: Firefox on the Linux host, in a window on Windows

Set by the user, 2026-08-13, as the thing both projects are for. Everything
below is a requirement, not an aspiration:

> run firefox on the linux host so that it appears as a window on windows with
> the taskbar icon correct, title correct, firefox being able to paint its UI
> elements into the windows titlebar, and hardware acceleration is working for
> the renderer and also for video decoding

## Why this is the right target

It is the only test that exercises both projects together, in the topology
they were built for, with a real application nobody wrote for us. Every piece
that exists so far is a component of it:

| requirement | what it exercises |
|---|---|
| Firefox on the Linux host | the app side, on a machine with no GPU |
| appears as a window on Windows | the compositor, and one HWND per toplevel |
| taskbar icon, title | `xdg_toplevel` metadata mapped to Win32 |
| UI painted into the titlebar | client-side decorations over a native frame |
| accelerated rendering | shared_vulkan, Zink, and the arena |
| accelerated video decode | Vulkan Video or a forwarded decode path |

## The topology, which is not the one either project was tested in

Read carefully, because it is a third arrangement:

- **Firefox runs on the Linux host.** It is a Wayland client.
- **The compositor runs on Windows, inside the guest.** It is the Wayland
  server.
- **The GPU is in the guest**, passed through, so shared_vulkan's GPU side is
  also there.

So the Wayland protocol has to cross the VM boundary — host to guest — which
neither project has done. shared_vulkan's cold path crosses that boundary
already (virtio-serial, and a host-only virtio-net link at <guest-nat-ip>), so the
byte stream is solved. What is not solved is what a `wl_shm` buffer means when
the client is on the other side of a hypervisor.

**The fd shim built in stage 1 does not survive this.** It sends a Windows
*section name*, which a Linux client cannot create and cannot name. Across the
VM boundary the buffer has to live somewhere both sides can reach, and that is
precisely what shared_vulkan's arena is. So the two projects meet here rather
than merely coexisting: **the arena is the buffer pool.**

That is worth stating plainly because it changes what "done" means for the
compositor. A compositor that only works with same-machine clients is not on
the path to this goal.

## The four requirements, and what each actually needs

### Taskbar icon and title — the easy pair, but not trivial

Title is `xdg_toplevel.set_title` → `SetWindowTextW`, already planned.

The icon is harder than it looks: Wayland has no icon protocol worth the name.
`xdg_toplevel.set_app_id` gives a desktop-entry name ("firefox"), and the icon
itself lives in a `.desktop` file and an icon theme **on the Linux side**. So
something must resolve app-id → icon on the host, ship the pixels across, and
set it with `WM_SETICON`. `xdg-toplevel-icon-v1` exists in newer
wayland-protocols and would do this properly if Firefox implements it — check
rather than assume.

### Firefox painting into the titlebar — the interesting one

This is Firefox's tabs-in-titlebar, and it is a genuine conflict with "native
Windows decorations":

- If we give the window a normal `WS_OVERLAPPEDWINDOW` frame, Windows owns the
  titlebar and Firefox cannot paint in it.
- If we let Firefox use client-side decorations, it paints its own titlebar —
  but then nothing is native, and the user asked for both.

The way Windows applications actually do this is
`DwmExtendFrameIntoClientArea` plus `WM_NCHITTEST` handling: the frame remains
the system's, and the client area is extended up into it so the application
draws there while Windows still provides the glass, the snap behaviour and the
buttons. That is the shape to aim for, and it means the compositor must
negotiate CSD with the client and then extend the frame — not simply pick one
of the two.

Note from the research: GTK does not implement `xdg-decoration`; it uses KDE's
`org_kde_kwin_server_decoration`. Firefox is GTK-based, so both protocols
matter, and which one Firefox actually honours needs measuring.

### Accelerated rendering

Firefox on Linux uses WebRender, which wants GL or Vulkan. The chain is
Firefox → GL → Zink → shared_vulkan → the guest's iGPU. Every link exists and
is measured: GL 4.6 through Zink at 98% of native frame rate.

What is new is that the swapchain has to end up in the compositor's window
rather than in an X window on the host. Presentation is currently app-side and
blits into an arena buffer — which is the right shape, since that buffer is
what the compositor needs to show.

### Accelerated video decode

The least explored of the four. Options are Vulkan Video through the forwarded
implementation, or a separate decode path. The phase 2 research called decode
the *best* fit for shared_vulkan, because decoded frames never have to cross
the arena — they are produced and consumed on the GPU side. Whether Firefox
can be pointed at it is a separate question from whether it works.

## What this does not require

Explicitly out of scope, so nobody builds it on the way:

- **DRM.** The user has said repeatedly to skip protected content, and the
  earlier analysis stands: licence acquisition cannot be forwarded because
  attestation exists to detect exactly that.
- **Every Wayland protocol.** Firefox needs a specific set. Enumerate what it
  actually binds and implement that, rather than the 137 the research
  classified.
