# Build this launcher on Linux. All runtime dependencies marked native* are
# Darwin outputs, guest* are Linux ELF outputs. No package is built on a Mac.
{ pkgs, guestPkgs, nativeBash, hl, sessionServices, compositor,
  guestIcd ? guestPkgs.callPackage ./icd.nix { },
  guestMesa ? guestPkgs.callPackage ./mesa-zink-va.nix { } }:
{ name, program, guestPackages ? [ ], waylandSocket ? "wayland-0", gpuSocket ? "sv-gpu",
  defaultArgs ? [ ] }:
assert pkgs.lib.assertMsg (builtins.match "[A-Za-z0-9][A-Za-z0-9._-]*" name != null)
  "Hyper Linux app name must be path-safe";
assert pkgs.lib.assertMsg (guestPkgs.stdenv.hostPlatform.system == "aarch64-linux")
  "Hyper Linux applications require ARM64 Linux packages";
let
  lib = pkgs.lib;
  closure = pkgs.closureInfo { rootPaths = [ program guestIcd guestMesa guestPkgs.glibc ] ++ guestPackages; };
  root = pkgs.runCommand "${name}-hl-sysroot" { } ''
    mkdir -p "$out/lib" "$out/dev/dri" "$out/sys/dev/char" "$out/sys/class/drm" \
      "$out/sys/bus/pci/devices" "$out/sys/devices/pci0000:00/0000:00:02.0/drm/renderD128"
    # The native /dev registry supplies the character-device identity and
    # operations; this directory supplies ordinary libdrm enumeration.
    touch "$out/dev/dri/renderD128"
    pci="$out/sys/devices/pci0000:00/0000:00:02.0"
    printf '0x1af4\n' > "$pci/vendor"
    printf '0x1050\n' > "$pci/device"
    printf '0x1af4\n' > "$pci/subsystem_vendor"
    printf '0x1050\n' > "$pci/subsystem_device"
    printf '0x030000\n' > "$pci/class"
    printf '0x00\n' > "$pci/revision"
    printf 'DRIVER=svdrm\nPCI_ID=1AF4:1050\nPCI_SUBSYS_ID=1AF4:1050\nPCI_SLOT_NAME=0000:00:02.0\n' > "$pci/uevent"
    printf '226:128\n' > "$pci/drm/renderD128/dev"
    printf 'MAJOR=226\nMINOR=128\nDEVNAME=dri/renderD128\n' > "$pci/drm/renderD128/uevent"
    ln -s ../../../bus/pci "$pci/subsystem"
    ln -s ../.. "$pci/drm/renderD128/device"
    ln -s ../../devices/pci0000:00/0000:00:02.0/drm/renderD128 "$out/sys/dev/char/226:128"
    ln -s ../../devices/pci0000:00/0000:00:02.0/drm/renderD128 "$out/sys/class/drm/renderD128"
    ln -s ../../../devices/pci0000:00/0000:00:02.0 "$out/sys/bus/pci/devices/0000:00:02.0"
    # This reference list keeps the complete ELF closure in the store. The
    # runtime binds /nix/store read-only at its original absolute path, so
    # PT_INTERP and DT_RUNPATH remain valid without flattening or patching ELF.
    cp ${closure}/store-paths "$out/store-paths"
    ln -s ${guestPkgs.glibc}/lib/ld-linux-aarch64.so.1 "$out/lib/ld-linux-aarch64.so.1"
  '';
  launcher = pkgs.writeTextFile {
    name = "${name}-hyperlinux";
    executable = true;
    destination = "/bin/${name}";
    text = ''
      #!${nativeBash}/bin/bash
      set -euo pipefail
      uid=$(id -u)
      session="$HOME/Library/Caches/waycompose/session-$uid"
      ${sessionServices}/bin/wc-macos-session-services --ensure
      [[ -d "$session" && -O "$session" && -f "$session/ready" ]] || {
        echo 'Hyper Linux native session services are not ready' >&2; exit 1;
      }
      # Native compositor endpoint, shared by all applications for this user.
      # Its own singleton/ensure command owns lifecycle, not the application.
      ${compositor}/bin/wc-hl-compositor ensure "$session" ${lib.escapeShellArg waylandSocket} ${lib.escapeShellArg gpuSocket}
      [[ -S "$session/${waylandSocket}" && -S "$session/${gpuSocket}" ]] || {
        echo 'Hyper Linux Wayland/GPU endpoints are not ready' >&2; exit 1;
      }
      mkdir -p "$session/tmp" "$session/shm" "$HOME/.local/state/waycompose/hl/${name}"
      guest_runtime="/run/user/$uid"
      unset DISPLAY XAUTHORITY DYLD_LIBRARY_PATH DYLD_INSERT_LIBRARIES
      export XDG_RUNTIME_DIR="$guest_runtime" TMPDIR=/tmp
      export WAYLAND_DISPLAY=${lib.escapeShellArg waylandSocket}
      export XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=waycompose
      export GDK_BACKEND=wayland QT_QPA_PLATFORM=wayland SDL_VIDEODRIVER=wayland MOZ_ENABLE_WAYLAND=1
      export DBUS_SESSION_BUS_ADDRESS="unix:path=$guest_runtime/bus"
      export PULSE_SERVER="unix:$guest_runtime/pulse/native" PULSE_COOKIE="$guest_runtime/pulse/cookie"
      export VK_DRIVER_FILES=${guestIcd}/share/vulkan/icd.d/sv_icd.json
      export VK_ICD_FILENAMES="$VK_DRIVER_FILES"
      export MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink
      export LIBGL_DRIVERS_PATH=${guestMesa}/lib/dri
      export __EGL_VENDOR_LIBRARY_FILENAMES=${guestMesa}/share/glvnd/egl_vendor.d/50_mesa.json
      export LIBVA_DRIVER_NAME=zink LIBVA_DRIVERS_PATH=${guestMesa}/lib/dri
      export LD_LIBRARY_PATH=${guestMesa}/lib:${guestIcd}/lib
      export SV_DRM_NODE=/dev/dri/renderD128
      export WC_HL_PRIME="$session/${waylandSocket}.prime"
      export WC_HL_RELAY="$session/${gpuSocket}.relay"
      export SV_SOCKET="$guest_runtime/${gpuSocket}"
      export SV_BELL="$guest_runtime/${gpuSocket}.bell"
      exec ${hl}/bin/hl --fs-mode=rooted --isolated --sysroot ${root} \
        --guest-home /home/user --guest-cwd /home/user \
        --bind "$HOME:/home/user" --bind "$session:$guest_runtime" \
        --bind "$session:$session" --bind "$session/shm:/dev/shm" --bind "$session/tmp:/tmp" \
        --bind "${root}/dev/dri:/dev/dri:ro" --bind "${root}/sys:/sys:ro" \
        --bind "/nix/store:/nix/store:ro" \
        ${lib.getExe program} ${lib.escapeShellArgs defaultArgs} "$@"
    '';
  };
in {
  inherit launcher root closure;
  app = { type = "app"; program = "${launcher}/bin/${name}"; };
}
