# Mesa with Zink's VA-API frontend (waycompose-design.md §112): video decode
# through the forwarded GPU's Vulkan Video, offered to applications as VA-API,
# which is the hardware decode path stock Firefox uses by default on Linux.
#
# Zink's frontend landed on Mesa main on 2026-10-16 (merge request 31517,
# H.264 only so far) after 26.2 branched, so this is main (pinned below, after that merge) rather
# than the release nixpkgs carries, plus the kopper patch every GPU session's
# Mesa already has (nix/mesa-kopper-wayland.patch). It is loaded per process
# (__EGL_VENDOR_LIBRARY_FILENAMES, LIBGL_DRIVERS_PATH, LIBVA_DRIVERS_PATH) and
# never installed as the system Mesa.
#
#   nix build --impure --expr '(builtins.getFlake "nixpkgs").legacyPackages
#     .x86_64-linux.callPackage ./nix/mesa-zink-va.nix {}'
{ mesa, fetchFromGitLab }:
mesa.overrideAttrs (old: {
  pname = "mesa-zink-va";
  version = "26.3.0-devel-42c325ea";
  src = fetchFromGitLab {
    domain = "gitlab.freedesktop.org";
    owner = "mesa";
    repo = "mesa";
    rev = "42c325ea02518276f08c5b44b487e37affc9fe69";
    hash = "sha256-R2WnUKhY6sg/tqGtA99DoClCOkQ3gwY1rxDVUcdFVbA=";
  };
  patches = (old.patches or [ ]) ++ [ ./mesa-kopper-wayland.patch ];
  # Two of nixpkgs' 26.2 flags main no longer takes as they are: the anti-lag
  # layer is gone, and Venus's protocol headers became a meson subproject,
  # which a sandboxed build cannot download (no GPU session uses Venus).
  mesonFlags = map (
    f:
    if builtins.substring 0 16 f == "-Dvulkan-layers="
    then "-Dvulkan-layers=device-select,intel-nullhw,overlay,screenshot"
    else if builtins.substring 0 17 f == "-Dvulkan-drivers="
    then builtins.replaceStrings [ ",virtio" ] [ "" ] f
    else f
  ) old.mesonFlags;
})
