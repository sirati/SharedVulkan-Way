# A minimal NixOS guest for exercising the **cross-VM** topology locally.
#
# design.md §13 steps 1–6 need no VM, and step 7 needs one. The only VM the
# project has had is `testhost`'s Windows guest, which is somebody's daily
# machine: it cannot be restarted, its configuration cannot be changed casually,
# and every experiment against it costs a round trip over a WiFi link. That is
# the wrong place to develop against and the right place to *confirm*.
#
# This guest is the development counterpart. It is Linux, it runs lavapipe, it
# can be killed and rebooted at will, and it is configured with exactly the four
# things the topology needs and nothing else:
#
#  1. **`memory-backend-memfd` with `share=on`.** The whole design rests on the
#     host being able to map the guest's RAM (§5.1). This is the one option that
#     makes that possible and it is not the NixOS VM module's default, so it is
#     forced here — see `virtualisation.qemu.options` below.
#  2. **A QMP socket**, so `sv-hostd` can read `info mtree -f` and build the
#     region table rather than computing one (R21).
#  3. **A virtio-serial port**, which is the cold path (D10) — the same
#     mechanism `testhost` already carries for `org.sirati.wifi.0`, so what is
#     proven here transfers rather than being a local convenience.
#  4. **lavapipe**, because the GPU side needs a Vulkan implementation and the
#     point of this rig is the platform layer, not the GPU. §5.4 calls lavapipe
#     "a great harness and a dangerous oracle" — it will import a pointer every
#     real driver rejects — so nothing measured here is evidence about memory
#     admissibility. It is evidence about locating, stitching, framing and the
#     handshake, which is what this rig is for.
#
# What is deliberately absent: no X, no sshd, no users, no persistent disk. The
# guest exists to run one program and print what happened.
{
  config,
  lib,
  pkgs,
  ...
}: {
  # The store comes from the host over 9p rather than being copied into a disk
  # image. That makes a rebuild-and-boot cycle seconds rather than minutes,
  # which is the entire reason this rig is worth having.
  virtualisation = {
    memorySize = 2048;
    cores = 2;
    diskSize = 1024;
    graphics = false;

    # The extra QEMU options are the whole configuration.
    qemu.options = [
      # (1) The guest's RAM must live in a memfd the host can open through
      # /proc/<qemu>/fd. `share=on` is what makes writes visible in both
      # directions; without it the mapping is private and the arena silently
      # becomes two copies — which is the failure mode this project must never
      # have, so it is asserted at run time as well.
      #
      # `-numa node,memdev=` is how the backend becomes the machine's RAM.
      # Passing `-object` alone attaches it to nothing.
      "-object memory-backend-memfd,id=svmem,size=2048M,share=on"
      "-numa node,memdev=svmem"

      # (2) QMP, for `info mtree -f`. A Unix socket in the run directory so the
      # harness can find it without a pid.
      "-qmp unix:\${SV_RUNDIR:-/tmp/sv-guest}/qmp.sock,server=on,wait=off"

      # (3) The cold path. `name=` is what the guest sees as
      # /dev/virtio-ports/<name>, and the host end is a Unix socket.
      "-device virtio-serial-pci,id=svserial"
      "-chardev socket,id=svcold,path=\${SV_RUNDIR:-/tmp/sv-guest}/cold.sock,server=on,wait=off"
      "-device virtserialport,bus=svserial.0,chardev=svcold,name=org.sirati.svcold.0"

      # A name the harness can match on, so `sv-hostd --vm` finds this QEMU and
      # not another one. `find_qemu_pid` refuses rather than guesses when two
      # match, which is exactly why the name has to be distinctive.
      "-name sv-guest,process=sv-guest"
    ];
  };

  # `memory-backend-memfd` replaces the machine's RAM, and the module's own
  # `-m` would otherwise describe a second, conflicting size.
  virtualisation.qemu.networkingOptions = lib.mkDefault [];

  environment.systemPackages = with pkgs; [
    mesa # lavapipe
    vulkan-loader
    vulkan-tools
  ];

  # lavapipe is the only implementation here, and naming it explicitly means a
  # run cannot silently pick up something else that happens to be installed.
  environment.variables = {
    VK_ICD_FILENAMES = "${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.x86_64.json";
    LD_LIBRARY_PATH = "${pkgs.vulkan-loader}/lib";
  };

  # A directory shared with the host, so the harness can hand the guest a
  # command without needing a shell in it.
  #
  # Driving the console was tried first and is the wrong shape: the console is
  # the only stdio the VM script offers, so a harness that reads the boot log
  # cannot also type into it, and adding an sshd to a VM whose whole purpose is
  # to be disposable is a service to configure for no gain. A file appearing in
  # a directory is a rendezvous both sides already understand — it is the same
  # one `gpu-workload-run.sh` uses over virtio-fs on the real machine.
  virtualisation.sharedDirectories.svrun = {
    source = "\${SV_RUNDIR:-/tmp/sv-guest}";
    target = "/sv";
  };

  # Run whatever the harness left for us, once, and report where the host can
  # see it. `Type = oneshot` with `RemainAfterExit` so a failure is visible in
  # the unit state rather than being restarted into a loop.
  systemd.services.sv-payload = {
    description = "the shared_vulkan guest half";
    wantedBy = ["multi-user.target"];
    after = ["local-fs.target"];
    path = with pkgs; [bash coreutils];
    # `environment.variables` below sets these for a *login shell*, and a systemd
    # unit is not one. Without them the payload dies with "no Vulkan loader:
    # libvulkan.so.1: cannot open shared object file" — which reads as a missing
    # package rather than a missing environment, and the package is installed.
    environment = {
      LD_LIBRARY_PATH = "${pkgs.vulkan-loader}/lib";
      VK_ICD_FILENAMES = "${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.x86_64.json";
    };
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      # /sv is the host's run directory, so stdout lands where the harness is
      # already looking.
      StandardOutput = "append:/sv/guest.log";
      StandardError = "append:/sv/guest.log";
    };
    script = ''
      echo "guest is up, waiting for /sv/guest-cmd.sh"
      for _ in $(seq 1 120); do
        [ -f /sv/guest-cmd.sh ] && break
        sleep 1
      done
      if [ ! -f /sv/guest-cmd.sh ]; then
        echo "no /sv/guest-cmd.sh appeared"
        exit 1
      fi
      echo "running:"; cat /sv/guest-cmd.sh
      exec bash /sv/guest-cmd.sh
    '';
  };

  # Autologin on the console, for looking at a guest that did not do what it
  # was told.
  services.getty.autologinUser = "root";
  users.users.root.password = "";

  # The guest half is handed in from the host's store, which the guest can see
  # because /nix/store is shared. So there is nothing to install: the harness
  # runs an absolute path.
  boot.kernelParams = ["console=ttyS0"];
  system.stateVersion = "25.05";
}
