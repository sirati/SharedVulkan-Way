# A shipped-library allocation integration. App and Mesa/Zink source stay
# upstream. The Wayland allocator chooses original transport pages before Cairo
# paints, using the authenticated broker's bounded region fd and its own lease.
{ gtk3, lib, rustPlatform, stdenv, wcNativeShm ? import ./wc-native-shm.nix { inherit lib rustPlatform stdenv; } }:
gtk3.overrideAttrs (old: {
  pname = "gtk3-native-shm";
  patches = (old.patches or []) ++ [ ./gtk-native-shm.patch ];
  buildInputs = (old.buildInputs or []) ++ [ wcNativeShm ];
  postPatch = (old.postPatch or "") + ''
    cp ${./gtk-native-shm-helper.c} gdk/wayland/gtk-native-shm-helper.c
    cp ${./gtk-native-shm-helper.h} gdk/wayland/gtk-native-shm-helper.h
    cp ${../crates/wc-proxy/protocols/wc-native-shm-v1.xml} gdk/wayland/protocol/wc-native-shm.xml
    substituteInPlace gdk/wayland/meson.build \
      --replace-fail "  'gdkdisplay-wayland.c'," "  'gdkdisplay-wayland.c', 'gtk-native-shm-helper.c'," \
      --replace-fail "  ['gtk-shell', 'private', ]," "  ['gtk-shell', 'private', ], ['wc-native-shm', 'private', ]," \
      --replace-fail "  shmlib," "  shmlib, cc.find_library('wc_native_shm', dirs: '${wcNativeShm}/lib'),"
    export NIX_CFLAGS_COMPILE="\''${NIX_CFLAGS_COMPILE:-} -I${wcNativeShm}/include"
  '';
})
