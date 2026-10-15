# Mesa that lets Zink present on a Wayland compositor which offers only wl_shm.
#
# Mesa's Wayland platform reaches Zink through one of two doors, and on this
# project's topology both are shut: the DRM door needs a render node from the
# compositor (dma-buf feedback or wl_drm), which wc-proxy cannot offer because
# no descriptor may cross the VM boundary (docs/design.md §1a); the swrast door
# opens only under LIBGL_ALWAYS_SOFTWARE, which Zink then reads as "accept only
# a CPU device". So on a wl_shm-only compositor Mesa loads llvmpipe whatever
# Vulkan driver is installed (docs/firefox-accel.md, "The Wayland WSI", §2).
#
# platform_x11.c already has the way out for the same situation on X11:
# LIBGL_KOPPER_DRI2. The patch gives the Wayland platform the same override and
# nothing else -- with Zink selected and that variable set, take the kopper path
# that presents through the Vulkan driver's own VK_KHR_wayland_surface.
# docs/design.md §0.28 has the measurements.
#
# It is the whole Mesa with one patch, not a cut-down one: nixpkgs builds Mesa
# with `auto_features=enabled`, and a driver list short enough to matter turns
# off things (the VA state tracker, rusticl) that the derivation then refuses to
# build without. It is loaded per process through __EGL_VENDOR_LIBRARY_FILENAMES
# and is never installed as the system Mesa.
#
# Build it against the *same* nixpkgs as the Firefox it is for, so that both
# load one libwayland-client: `scripts/sv-firefox-wayland.sh` does
#   nix build --impure --expr '(builtins.getFlake "nixpkgs").legacyPackages
#     .x86_64-linux.callPackage ./nix/mesa-kopper-wayland.nix {}'
{ mesa }:
mesa.overrideAttrs (old: {
  pname = "mesa-kopper-wayland";
  patches = (old.patches or [ ]) ++ [ ./mesa-kopper-wayland.patch ];
})
