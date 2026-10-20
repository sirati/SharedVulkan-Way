# Disposable kernel proof. This never touches the family Windows VM or host DRM.
{ bins, pkgs ? let n = (builtins.fromJSON (builtins.readFile ../flake.lock)).nodes.nixpkgs.locked;
  in import (fetchTarball { url = "https://github.com/NixOS/nixpkgs/archive/${n.rev}.tar.gz"; sha256 = n.narHash; }) { system = "x86_64-linux"; } }:
let binDir = builtins.path { path = bins; name = "native-shm-kernel-test"; };
in pkgs.testers.runNixOSTest {
  name = "native-gtk-original-shm";
  nodes.host = { ... }: {
    boot.kernelModules = [ "udmabuf" ];
    boot.kernel.sysctl."kernel.yama.ptrace_scope" = 1;
    services.udev.extraRules = ''KERNEL=="udmabuf", GROUP="kvm", MODE="0660"'';
    users.users.alice = { isNormalUser = true; extraGroups = [ "kvm" ]; };
    virtualisation.memorySize = 1024;
  };
  testScript = ''
    start_all()
    host.wait_for_unit("multi-user.target")
    host.succeed("udevadm settle; test -c /dev/udmabuf")
    host.succeed("su - alice -c '${binDir}/wc-native-shm-tests --ignored --exact unix::tests::real_udmabuf_original_pages_identity_bounds_and_two_broker_leases --nocapture'")
  '';
}
