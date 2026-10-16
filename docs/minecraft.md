# Minecraft through the forwarder

The goal, set 2026-10-16: run Minecraft on the Linux VM host and play it on the
Windows guest — its window on the guest's desktop through waycompose, its
rendering on the guest's Intel iGPU through shared_vulkan — in two forms that
must both work:

* **stock**, which renders OpenGL; on this topology that is Zink, Mesa's
  OpenGL on Vulkan, on our ICD, the same chain Firefox's WebRender takes
  (docs/design.md §0.27–§0.29, `firefox-accel.md`);
* **VulkanMod**, a Fabric mod that replaces the renderer with Vulkan, on our
  ICD directly.

No Microsoft account is used for any of it: the game runs in Mojang's demo
mode, launched by Prism Launcher's own demo path.

This file is the user's view and the procedure. waycompose-design.md §96 is
the engineering record: what broke, why, and what was changed.

## What runs where

| | on the workstation (Phase 1) | on testhost and the guest (Phase 2) |
|---|---|---|
| the game | Linux, a native **Wayland** GLFW client | the VM host |
| compositor | `wc-compositor.exe` under Wine on an Xvfb, behind `wc-proxy` | `sv-guest.exe` on the guest's desktop, behind `wc-proxy` |
| GPU side | `sv-gpu-server` on the AMD iGPU (RADV, `renderD129`) | `sv-guest.exe`'s GPU half on the Intel iGPU |
| input | `xdotool` into the Xvfb → Wine → Win32 messages | a person, or TeamViewer |

The workstation's NVIDIA card is never touched: the GPU side is shown only
`radeon_icd`, the game's loader only our ICD, and the game runs with an empty
`/dev/dri` (see "Things that bite" below).

## Versions

| | |
|---|---|
| Prism Launcher | 11.1.1 (`nixpkgs#prismlauncher`) |
| Java | OpenJDK 21.0.12 (`nixpkgs#jdk21`) |
| Minecraft | 1.21.11, demo (`--demo`) |
| Fabric loader | 0.19.5 (intermediary 1.21.11) |
| VulkanMod | 0.6.8+1.21.11 from Modrinth, sha1 `f273dc13…` — the latest release for 1.21.x; the 26.1 builds need Java 25 |
| LWJGL / GLFW | LWJGL 3.3.3; GLFW is nixpkgs' `glfw3-minecraft` 3.4, through Prism's "use system GLFW" |
| GL | `nix/mesa-kopper-wayland.nix` (Mesa 26.2.4 with the kopper-on-Wayland override, §0.28) |

## Demo mode, and how Prism gets there

Prism will not start without a setup wizard whose last page offers a Microsoft
login, and will not launch without asking about accounts. Its demo path, with
no account at all, is four dialogs:

1. **Quick Setup** ("Add Microsoft account"): **Finish**, without adding one.
2. **No Accounts**: **No**.
3. **Play demo?**: **Play Demo**.
4. **Player name**: **OK** (any name; it is the demo player's).

Prism then launches the game with `--demo` and an access token of `0`, which is
what Mojang's launcher does for a demo. The game says `Minecraft 1.21.11 Demo`
on its title screen and offers "Play Demo World".

`scripts/sv-minecraft.sh setup` clicks those four buttons on an Xvfb of its own,
by position. **It clicks rather than pressing keys**, because the wizard's
focused widget is "Add Microsoft account": a Return or a Space that reaches it
starts Microsoft's device-code login. That happened twice while the dialogs were
first automated by keyboard. Prism polled `login.microsoftonline.com` for a code
and tried to open a browser; nothing was entered anywhere and no account
exists, but the script now also runs Prism with a refusing `xdg-open`, no
session bus, and a watchdog that kills Prism the moment its log mentions a
Microsoft login.

## Running it (Phase 1)

Inside `nix develop`, from the repository:

```
cargo build --release -p sv-icd -p sv-gpu-server -p wc-proxy
cargo build --target x86_64-pc-windows-gnu -p wc-compositor
scripts/sv-minecraft.sh setup                # once: Prism, the game, Fabric, VulkanMod
scripts/sv-minecraft.sh run stock 180        # play it, and judge what happened
scripts/sv-minecraft.sh run vulkanmod 180
```

`setup` makes `/tmp/svmc/prism` (Prism's data dir; never the user's
`~/.local/share/PrismLauncher`), two instances `mc-stock` and `mc-vulkanmod`,
downloads VulkanMod from Modrinth and checks its sha1, and launches each instance
once through Prism's demo path so that Prism downloads the game, Fabric and their
libraries. Prism's *wrapper command* records the launch it made — argv,
environment and the launch script it writes to the game's stdin — and `run`
replays that launch, with the session's environment, so a measurement does not
depend on clicking through dialogs. The replay is Prism's launch byte for byte:
the same `NewLaunch.jar`, classpath, `-Dorg.lwjgl.glfw.libname` and `--demo`.

`run` starts the compositor under Wine, `wc-proxy`, the GPU side on RADV, and
the game, then plays it with `xdotool` the way a person would: into the world,
"Continue Playing!", F3, 300 px of mouse to the right and 150 px up, two seconds
of W, Space, F2 (Minecraft's own screenshot), F11 and back, 30 s untouched for
the frame rate, Escape, and the window's close button. The verdict is in its
output and its exit status. Options:

| | |
|---|---|
| `SVMC_VALIDATE=1` | `VK_LAYER_KHRONOS_validation` on the GPU side, VUIDs counted |
| `SVMC_APP_VALIDATE=1` | the layer inside the game too (Zink crashes under it on native RADV as well; a diagnostic only) |
| `SVMC_VSYNC=1` | leave the game's vsync on |
| `SVMC_FULLSCREEN=1` | F11 and back |
| `SVMC_SOUND=1` | this desktop's PulseAudio for the game, at master volume 0 |
| `SVMC_GAME_ENV` | extra `NAME=value` words for the game (`SV_DEBUG=1`) |

## What was measured on the workstation

2026-10-16, AMD Ryzen 9 9950X3D's iGPU (RADV RAPHAEL_MENDOCINO, Mesa 26.2.0 on
the GPU side), the game at 854x480 windowed and 1600x1000 fullscreen, render
distance 8, vsync off, `maxFps` unlimited, the 30 s steady window of each run.
The frame rate is `icd.present`, our ICD's own trace of every
`vkQueuePresentKHR` the game made (§88's frame timeline).

| | renderer (F3) | frames/s | present interval p50 / p95 / p99 |
|---|---|---|---|
| stock | `AMD Ryzen 9 9950X3D … (RADV RAPHAEL_MENDOCINO) (MESA_RADV)`, `OpenGL 4.6 (Core Profile) Mesa 26.2.4`, Display `(Mesa)` | 238 | 3.0 / 8.7 / 17.1 ms |
| VulkanMod | `AMD Ryzen 9 9950X3D … (RADV RAPHAEL_MENDOCINO)`, `Vulkan 1.4.0`, `VulkanMod 0.6.8+1.21.11`, Display `(AMD)` | 347 | 2.0 / 8.1 / 11.7 ms |
| stock, vsync on | | 32 | 19.4 / 71.2 / 114.8 ms |
| VulkanMod, vsync on | | 195 | 3.1 / 17.8 / 37.2 ms |

With vsync on, stock is paced by Mesa's Wayland swap interval (it waits for
`wl_surface.frame`) and runs at half the compositor's 60 Hz clock with a long
tail; the frame timeline puts most of each frame in GPU waits, which points at
the iGPU's clocks under a light load rather than at the path. It is the number
to measure again on the guest. VulkanMod's vsync is a present mode, and our
ICD's `FIFO` is not tied to a display, so it is not capped.

Both, every run: the window on `wc-compositor.exe`, keyboard (F3, W, Space,
F2, F11, Escape), **mouse-look through the pointer lock** (Facing changed by the
motion sent), fullscreen and back, sound through OpenAL to PulseAudio, the F2
screenshot written, and the close button quitting the game cleanly (exit 0,
`Stopping!`). The run's pictures are in the run directory it prints.

The VUID count on the GPU side (`SVMC_VALIDATE=1`): **none** for VulkanMod;
for stock, `VUID-vkCmdBeginRendering-pRenderingInfo-09592` and
`VUID-vkCmdDrawMultiEXT-imageLayout-00344`, a handful each, around F2, F11 and
world loading. The first is Zink's own: it reproduces with Zink on this RADV
**without** the forwarder, under sway, with the layer inside the game (§96.6).

## Things that bite

* **LWJGL's own GLFW crashes in `glfwInit`** on a compositor with no `wl_seat`,
  which a headless pixman weston is — so weston, the usual control, cannot run
  Minecraft at all. waycompose always has a seat. Prism's "use system GLFW"
  (`UseNativeGLFW` in the instance) gives nixpkgs' `glfw3-minecraft`, which the
  instances use; it has the same `wl_seat` assumption.
* **GLFW chooses Wayland by itself** when `WAYLAND_DISPLAY` is set and
  `DISPLAY` is not; nothing has to force it.
* **The narrator kills the JVM without PulseAudio.** Mojang's Linux narrator
  speaks through `pa_simple` and aborts (`Assertion 'p' failed …
  pa_simple_write`) when there is no server. The harness turns it off; a
  session without audio should too.
* **Mesa's EGL probes every DRM device** before it settles on kopper, and on
  the workstation the first one is the NVIDIA card. The harness runs the game
  under `bwrap` with an empty `/dev/dri`, which is also what testhost has.
* **OpenAL tries PipeWire first** and, with the session's private
  `XDG_RUNTIME_DIR`, fails (`errno 112`) and falls back to PulseAudio, which
  works with `PULSE_SERVER` set. On the workstation the default sink is
  `Dummy Output`.
* **The first world takes a while.** Creating the demo world takes about a
  minute, and with the GPU-side layer longer.

## Phase 2

Recorded in waycompose-design.md §96 when it is done.
