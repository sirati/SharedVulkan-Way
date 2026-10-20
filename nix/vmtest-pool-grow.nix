# The GPU pool grows past its boot device and gives memory back
# (waycompose-design.md §108.13), end to end, with the guest-RAM regions
# handed out by the broker (§108.18). Run by scripts/sv-pool-grow-e2e.sh.
#
#   wcbroker   `sv-gpu-server pool-guest` on lavapipe: the stand-in for the
#              Windows guest's GPU process, growing from a sealed memfd that
#              stands in for the guest's RAM (scattered, stamped 2 MiB units);
#              and the broker daemon, which alone can make udmabufs of it
#   alice      the client side: sv-hostd's cold path, and Vulkan clients
#              (sv-pool-stress, vulkaninfo, Zink) through the ICD, which gets
#              every grown region from the broker
#
# The boot device's file (`/run/wcpool/gpu`, region 0) is group-readable by
# alice here: it stands for the BAR, which in a product session the daemon
# publishes to the session's user (§111.10). Guest RAM never is.
{
  bins,
  pkgs ? let
    n = (builtins.fromJSON (builtins.readFile ../flake.lock)).nodes.nixpkgs.locked;
  in
    import (fetchTarball {
      url = "https://github.com/NixOS/nixpkgs/archive/${n.rev}.tar.gz";
      sha256 = n.narHash;
    }) {system = "x86_64-linux";},
  budgetMiB ? 768,
}: let
  binDir = builtins.path {
    path = bins;
    name = "sv-pool-grow-bins";
  };
  lvp = "${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.x86_64.json";
  icdJson = pkgs.writeText "sv_icd.json" (builtins.toJSON {
    file_format_version = "1.0.1";
    ICD = {
      library_path = "${binDir}/libsv_icd.so";
      api_version = "1.4.0";
      library_arch = "64";
      is_portability_driver = false;
    };
  });
  probes = pkgs.runCommandCC "sv-pool-grow-probes" {buildInputs = [pkgs.vulkan-loader pkgs.vulkan-headers pkgs.libGL pkgs.libglvnd];} ''
    mkdir -p $out/bin
    cc -O1 -o $out/bin/gl-probe ${../scripts/gl-probe.c} -lEGL -lGL -lm
    cc -O1 -o $out/bin/sv-pool-stress ${../scripts/sv-pool-stress.c} -lvulkan
  '';
  libs = pkgs.lib.makeLibraryPath [pkgs.vulkan-loader pkgs.libGL pkgs.libglvnd];
  # The client side's run, as alice: the steps of the old local harness.
  clientRun = pkgs.writeShellScript "pool-grow-client" ''
    set -u
    export PATH=${pkgs.lib.makeBinPath [pkgs.coreutils pkgs.gnugrep pkgs.vulkan-tools probes]}
    export LD_LIBRARY_PATH=${libs}
    R=/run/vmt
    ${binDir}/sv-hostd ivshmem-serve --region /run/wcpool/gpu --cold pool --socket $R/icd.sock >$R/hostd.log 2>&1 &
    for _ in $(seq 1 50); do [ -S $R/icd.sock ] && break; sleep 0.1; done
    export VK_DRIVER_FILES=${icdJson} SV_SOCKET=$R/icd.sock WC_BROKER=/run/waycompose/broker.sock
    timeout 600 sv-pool-stress 64 10 big 75 >$R/stress-big.log 2>&1 & A=$!
    sleep 4
    MESA_LOADER_DRIVER_OVERRIDE=zink ZINK_DEBUG=cpu EGL_PLATFORM=surfaceless timeout 180 gl-probe >$R/gl1.log 2>&1; echo $? >$R/gl1.rc
    timeout 120 vulkaninfo --summary >$R/vulkaninfo.log 2>&1; echo $? >$R/vulkaninfo.rc
    wait $A; echo $? >$R/stress-big.rc
    : >$R/client-first.done
  '';
  clientAgain = pkgs.writeShellScript "pool-grow-again" ''
    export PATH=${pkgs.lib.makeBinPath [pkgs.coreutils probes]}
    export LD_LIBRARY_PATH=${libs}
    export VK_DRIVER_FILES=${icdJson} SV_SOCKET=/run/vmt/icd.sock WC_BROKER=/run/waycompose/broker.sock
    timeout 600 sv-pool-stress 64 0 again >/run/vmt/stress-again.log 2>&1; echo $? >/run/vmt/stress-again.rc
  '';
in
  pkgs.testers.runNixOSTest {
    name = "sv-pool-grow-broker";
    nodes.host = {...}: {
      # The pool fills to the budget once; the memfd holds what is touched.
      virtualisation.memorySize = 768 + budgetMiB;
      virtualisation.cores = 4;
      boot.kernelModules = ["udmabuf"];
      services.udev.extraRules = ''
        KERNEL=="udmabuf", GROUP="kvm", MODE="0660"
      '';
      users.groups.wcbroker = {};
      users.groups.wcgpu = {};
      users.users.wcbroker = {
        isSystemUser = true;
        group = "wcbroker";
        extraGroups = ["kvm" "wcgpu"];
      };
      users.users.alice = {
        isNormalUser = true;
        uid = 1001;
        extraGroups = ["wcgpu"];
      };
      hardware.graphics.enable = true;
      systemd.tmpfiles.rules = ["d /run/vmt 1777 root root -"];

      systemd.services.wcgpu = {
        description = "the guest's GPU process, growing from a memfd standing in for its RAM";
        wantedBy = ["multi-user.target"];
        environment = {
          VK_DRIVER_FILES = lvp;
          LD_LIBRARY_PATH = libs;
        };
        serviceConfig = {
          User = "wcbroker";
          Group = "wcgpu";
          RuntimeDirectory = "wcpool";
          RuntimeDirectoryMode = "0750";
          UMask = "0007";
          ExecStartPre = "${pkgs.coreutils}/bin/truncate -s 64M /run/wcpool/gpu";
          ExecStart = "${binDir}/sv-gpu-server pool-guest --pool-file /run/wcpool/gpu --grow-ram memfd:${toString (budgetMiB + 256)} --grow-budget ${toString budgetMiB} --sessions 8 --idle 20";
          ExecStartPost = "${pkgs.bash}/bin/sh -c 'echo $MAINPID > /run/wcpool/gpu.pid'";
        };
      };

      systemd.services.wcbroker = {
        description = "the broker daemon with its guest-RAM component";
        wantedBy = ["multi-user.target"];
        after = ["wcgpu.service"];
        requires = ["wcgpu.service"];
        serviceConfig = {
          User = "wcbroker";
          Group = "wcbroker";
          RuntimeDirectory = "waycompose";
          RuntimeDirectoryMode = "0755";
          StateDirectory = "wcbroker";
          StateDirectoryMode = "0700";
          ExecStartPre = [
            "${pkgs.coreutils}/bin/truncate -s 1M /var/lib/wcbroker/arena"
            "${pkgs.coreutils}/bin/mkdir -p /var/lib/wcbroker/installs"
          ];
          ExecStart = builtins.concatStringsSep " " [
            "${binDir}/wc-arena-host sessiond"
            "--ivshmem /var/lib/wcbroker/arena"
            "--installs /var/lib/wcbroker/installs --state /var/lib/wcbroker"
            "--broker /run/waycompose/broker.sock"
            "--guest-ram-boot /run/wcpool/gpu --guest-ram-qemu-pidfile /run/wcpool/gpu.pid"
            "--guest-ram-user 1001"
          ];
        };
      };
    };

    testScript = ''
      start_all()
      host.wait_for_unit("wcgpu.service")
      host.wait_for_unit("wcbroker.service")
      host.wait_until_succeeds("journalctl -u wcgpu | grep -q 'publishing the extent'", timeout=60)
      host.wait_until_succeeds("journalctl -u wcbroker | grep -q 'pool gpu: bootstrap'", timeout=60)
      host.succeed("systemd-run --uid=alice --unit=client ${clientRun}")
      host.wait_until_succeeds("grep -q lingering /run/vmt/stress-big.log", timeout=600)
      grown = int(host.succeed("journalctl -u wcgpu | grep -c 'added to the pool' || true").strip())
      print(f"{grown} region(s) added")
      # the regions go back while the load still lives (all but the low-water margin)
      host.wait_until_succeeds(f"[ $(journalctl -u wcgpu | grep -c 'given back to the guest') -ge {grown - 1} ]", timeout=90)
      alive = host.execute("systemctl is-active client.service")[0] == 0
      host.wait_until_succeeds("[ -e /run/vmt/client-first.done ]", timeout=600)
      host.succeed("systemd-run --uid=alice --wait --unit=again ${clientAgain}")

      def rc(name):
          return host.succeed(f"cat /run/vmt/{name}.rc").strip()

      print(host.succeed("grep RESULT /run/vmt/stress-*.log"))
      first = int(host.succeed("sed -n 's/.*RESULT label=big block=64 first=\\([0-9]*\\).*/\\1/p' /run/vmt/stress-big.log").strip() or "0")
      assert rc("stress-big") == "0" and "ok=1" in host.succeed("cat /run/vmt/stress-big.log"), "the load"
      assert first >= 9, f"the load filled past the boot pool: {first} x 64 MiB"
      assert rc("gl1") == "0", "Zink drew on the grown pool"
      host.succeed("grep -qi zink /run/vmt/gl1.log")
      assert rc("vulkaninfo") == "0", "vulkaninfo"
      host.succeed("grep -q llvmpipe /run/vmt/vulkaninfo.log")
      assert grown >= 2, f"regions added on demand: {grown}"
      assert alive, "the regions went back while the load lived"
      assert rc("stress-again") == "0", "the second load grew the pool again"
      host.succeed("grep -q ok=1 /run/vmt/stress-again.log")
      host.fail("grep -qi 'DEVICE_LOST\\|lost device' /run/vmt/*.log")
      host.fail("journalctl -u wcbroker | grep -q REFUSED")
      host.succeed("journalctl -u wcbroker | grep -q 'drained; the broker.s udmabufs are closed'")
      print(host.succeed("journalctl -u wcbroker --no-pager | grep guest-ram | tail -40"))
      print(host.succeed("journalctl -u wcgpu --no-pager | grep -E 'regions:|added to the pool|given back' | tail -30"))
    '';
  }
