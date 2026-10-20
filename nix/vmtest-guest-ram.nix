# The guest-RAM broker as a NixOS VM test (waycompose-design.md §108.18,
# security-model.md R43, R44). Run by scripts/wc-guestram-vmtest.sh, which
# builds the two static binaries and passes their directory as `bins`.
#
#   host (this VM)        wcbroker   the broker daemon (`wc-arena-host sessiond
#                                    --broker` with its guest-RAM component),
#                                    and the nested QEMU: the only user with
#                                    /dev/udmabuf (group kvm), the RAM memfd
#                                    and /proc/<qemu>
#                         alice      a client: gets her regions, nothing else
#                         bob        another client: gets none of alice's
#   nested guest                     a kernel and an initramfs; the GPU
#                                    pool's provider grows from hugetlb pages
#                                    of its RAM, which is QEMU's memfd
#
# The test driver (root) talks to the provider through a control page in the
# shared bootstrap, so nothing in the test goes over a network.
{
  bins,
  pkgs ? let
    n = (builtins.fromJSON (builtins.readFile ../flake.lock)).nodes.nixpkgs.locked;
  in
    import (fetchTarball {
      url = "https://github.com/NixOS/nixpkgs/archive/${n.rev}.tar.gz";
      sha256 = n.narHash;
    }) {system = "x86_64-linux";},
}: let
  binDir = builtins.path {
    path = bins;
    name = "wc-guestram-vmtest-bins";
  };
  # The nested guest's RAM, and the hugetlb pages it may grow from
  # (64 x 2 MiB). Measured need: the kernel and this initramfs fit in well
  # under 100 MiB beside the pages.
  guestMiB = 320;
  hugePages = 64;
  nestedInit = pkgs.writeScript "init" ''
    #!/bin/busybox sh
    /bin/busybox --install -s /bin
    mount -t proc p /proc; mount -t sysfs s /sys; mount -t devtmpfs d /dev
    echo "nested: $(cat /proc/sys/kernel/osrelease), hugepages $(cat /proc/sys/vm/nr_hugepages)"
    exec /bin/sv-ram-vmtest provider
  '';
  nestedInitrd = pkgs.makeInitrd {
    compressor = "gzip";
    contents = [
      {
        object = nestedInit;
        symlink = "/init";
      }
      {
        object = "${pkgs.pkgsStatic.busybox}/bin/busybox";
        symlink = "/bin/busybox";
      }
      {
        object = "${binDir}/sv-ram-vmtest";
        symlink = "/bin/sv-ram-vmtest";
      }
    ];
  };
in
  pkgs.testers.runNixOSTest {
    name = "wc-guestram-broker";
    nodes.host = {config, ...}: let
      kernel = config.boot.kernelPackages.kernel;
    in {
      # Measured: the nested guest, QEMU and this NixOS with margin.
      virtualisation.memorySize = 1024;
      virtualisation.cores = 2;
      virtualisation.qemu.options = ["-cpu host"];
      boot.kernelModules = ["udmabuf" "kvm-intel" "kvm-amd"];
      services.udev.extraRules = ''
        KERNEL=="udmabuf", GROUP="kvm", MODE="0660"
      '';
      users.groups.wcbroker = {};
      users.users.wcbroker = {
        isSystemUser = true;
        group = "wcbroker";
        # /dev/udmabuf and /dev/kvm. Only this account; the clients are not
        # in it.
        extraGroups = ["kvm"];
      };
      users.users.alice = {
        isNormalUser = true;
        uid = 1001;
      };
      users.users.bob = {
        isNormalUser = true;
        uid = 1002;
      };
      environment.systemPackages = [binDir];
      environment.variables.PATH = ["${binDir}"];
      systemd.tmpfiles.rules = ["d /run/vmt 1777 root root -"];

      systemd.services.wcvm = {
        description = "the nested guest whose RAM the broker serves";
        wantedBy = ["multi-user.target"];
        serviceConfig = {
          User = "wcbroker";
          Group = "wcbroker";
          RuntimeDirectory = "wcvm";
          RuntimeDirectoryMode = "0700";
          ExecStartPre = "${pkgs.coreutils}/bin/truncate -s 1M /run/wcvm/gpu";
          ExecStart = builtins.concatStringsSep " " [
            "${pkgs.qemu_kvm}/bin/qemu-system-x86_64"
            "-machine pc,accel=kvm:tcg,memory-backend=memory-backend-ram"
            "-m ${toString guestMiB}"
            "-object memory-backend-memfd,id=memory-backend-ram,size=${toString guestMiB}M,share=on"
            "-object memory-backend-file,id=bar,mem-path=/run/wcvm/gpu,size=1M,share=on"
            "-device ivshmem-plain,memdev=bar"
            "-kernel ${kernel}/bzImage"
            "-initrd ${nestedInitrd}/initrd"
            "-append 'console=ttyS0 hugepages=${toString hugePages} panic=-1'"
            "-display none -serial file:/run/wcvm/console.log -nodefaults"
            "-pidfile /run/wcvm/qemu.pid"
          ];
        };
      };

      systemd.services.wcbroker = {
        description = "the broker daemon with its guest-RAM component";
        wantedBy = ["multi-user.target"];
        after = ["wcvm.service"];
        requires = ["wcvm.service"];
        serviceConfig = {
          User = "wcbroker";
          Group = "wcbroker";
          RuntimeDirectory = "waycompose";
          RuntimeDirectoryMode = "0755";
          RuntimeDirectoryPreserve = "no";
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
            "--guest-ram-boot /run/wcvm/gpu --guest-ram-qemu-pidfile /run/wcvm/qemu.pid"
            "--guest-ram-user 1001"
          ];
        };
      };
    };

    testScript = ''
      import time
      start_all()
      host.wait_for_unit("wcvm.service")
      host.wait_for_unit("wcbroker.service")
      host.wait_until_succeeds("grep -q 'control page' /run/wcvm/console.log", timeout=120)
      host.wait_until_succeeds("journalctl -u wcbroker | grep -q 'pool gpu: bootstrap'", timeout=60)
      ctl = "${binDir}/sv-ram-vmtest ctl --boot /run/wcvm/gpu "

      def client(name, user, pool, extra=""):
          host.succeed(f"touch /run/vmt/{name}.hold; chmod 666 /run/vmt/{name}.hold")
          host.succeed(f"systemd-run --uid={user} --unit=c-{name} -p StandardOutput=file:/run/vmt/{name}.log "
                       f"-p StandardError=file:/run/vmt/{name}.log -E WC_BROKER=/run/waycompose/broker.sock "
                       f"${binDir}/sv-ram-vmtest client map {pool} --until /run/vmt/{name}.hold {extra}")

      def mapped(name):
          host.wait_until_succeeds(f"grep -q MAPPED /run/vmt/{name}.log", timeout=30)
          return host.succeed(f"cat /run/vmt/{name}.log")

      def release(name):
          host.succeed(f"rm /run/vmt/{name}.hold")
          host.wait_until_succeeds(f"grep -q STABLE /run/vmt/{name}.log", timeout=30)
          return host.succeed(f"cat /run/vmt/{name}.log")

      def status(k):
          return host.succeed(ctl + f"status {k}").strip()

      with subtest("growth: a region of the guest's RAM, located, mapped, published for alice only"):
          print(host.succeed(ctl + "grow 32M"))
          host.succeed("journalctl -u wcbroker | grep -q 'published as gpu.1, broker region .* for uid 1001'")
          client("a1", "alice", "gpu.1")
          print(mapped("a1"))
          # zero-copy: what alice wrote through her udmabufs is in the guest's pages
          print(host.succeed(ctl + "check 1"))

      with subtest("bob gets none of alice's regions"):
          host.succeed("su bob -c 'WC_BROKER=/run/waycompose/broker.sock ${binDir}/sv-ram-vmtest client map gpu.1 --until /nonexistent --expect-fail'")

      with subtest("a client reaches no raw resource"):
          pid = host.succeed("cat /run/wcvm/qemu.pid").strip()
          memfd = host.succeed(f"ls -l /proc/{pid}/fd | grep -m1 memfd:memory-backend | awk '{{print $9}}'").strip()
          for p in ["/dev/udmabuf", f"/proc/{pid}/fd/{memfd}", f"/proc/{pid}/mem", "/run/wcvm/gpu", "/var/lib/wcbroker/arena"]:
              host.succeed(f"su alice -c '${binDir}/sv-ram-vmtest client open {p}'")
          host.fail(f"su alice -c 'ls /proc/{pid}/fd'")
          host.fail("id -nG alice | grep -qw kvm")

      with subtest("more growth and a shrink, while alice's mapping stays where it is"):
          print(host.succeed(ctl + "grow 64M"))
          print(host.succeed(ctl + "grow 16M"))
          client("a2", "alice", "gpu.2")
          mapped("a2")
          print(release("a2"))
          print(host.succeed(ctl + "retire 2"))
          host.wait_until_succeeds(ctl + "status 2 | grep -q freed", timeout=30)
          host.succeed("grep -q 'region 2 freed' /run/wcvm/console.log")

      with subtest("a retire is refused while the guest holds a lease (an import) in the region"):
          out = host.succeed(ctl + "lease 1 4M")
          off = out.split("offset=")[1].split()[0]
          host.fail(ctl + "retire 1")
          host.succeed(ctl + f"release {off}")

      with subtest("a retire waits while a host lease lives, and completes when it ends"):
          print(host.succeed(ctl + "retire 1"))
          time.sleep(3)
          assert status(1) == "retiring", status(1)
          host.succeed("journalctl -u wcbroker | grep -q 'region 1 (epoch .*): retiring'")
          host.fail("journalctl -u wcbroker | grep -q 'UNMAPPED region 1'")
          out = release("a1")
          print(out)
          assert "STABLE gpu.1" in out
          host.wait_until_succeeds(ctl + "status 1 | grep -q freed", timeout=30)
          host.succeed("journalctl -u wcbroker | grep -q 'UNMAPPED region 1'")

      with subtest("the daemon restarts while alice maps a region: the region waits for her"):
          client("a3", "alice", "gpu.3")
          mapped("a3")
          host.succeed("sleep 1")  # one beat records the holder
          host.succeed("systemctl restart wcbroker.service")
          host.wait_until_succeeds("journalctl -u wcbroker | grep -q 'published by the previous run'", timeout=60)
          print(host.succeed(ctl + "retire 3"))
          time.sleep(3)
          assert status(3) == "retiring", status(3)
          host.succeed("journalctl -u wcbroker | grep -q 'held for the previous run'")
          print(release("a3"))
          host.wait_until_succeeds(ctl + "status 3 | grep -q freed", timeout=30)

      for k, damage in [(4, "unreadable"), (5, "malformed")]:
          with subtest(f"{damage} recovery state cannot release alice's existing mapping"):
              print(host.succeed(ctl + "grow 16M"))
              name = f"recovery-{k}"
              client(name, "alice", f"gpu.{k}")
              print(mapped(name))
              journal = "/var/lib/wcbroker/guest-ram/gpu.regions"
              backup = f"/run/vmt/recovery-{k}.saved"
              host.succeed("systemctl stop wcbroker.service")
              host.succeed(f"cp {journal} {backup}")
              if damage == "unreadable":
                  host.succeed(f"chmod 000 {journal}")
              else:
                  host.succeed(f"printf 'region broken\\n' > {journal}")
              host.succeed("systemctl start wcbroker.service")
              invocation = host.succeed("systemctl show -p InvocationID --value wcbroker.service").strip()
              host.wait_until_succeeds(f"journalctl _SYSTEMD_INVOCATION_ID={invocation} | grep -q 'recovery blocked'", timeout=30)
              print(host.succeed(ctl + f"retire {k}"))
              time.sleep(3)
              assert status(k) == "retiring", status(k)
              host.fail(f"journalctl _SYSTEMD_INVOCATION_ID={invocation} | grep -q 'UNMAPPED region {k}'")
              if damage == "unreadable":
                  host.succeed(f"cmp {journal} {backup}; test $(stat -c %a {journal}) = 0")
              else:
                  assert host.succeed(f"cat {journal}") == "region broken\n"
              # Restore the trusted record while alice still maps the same
              # pages. A new daemon must wait for her recorded process too.
              host.succeed("systemctl stop wcbroker.service")
              host.succeed(f"cp {backup} {journal}; chown wcbroker:wcbroker {journal}; chmod 600 {journal}")
              host.succeed("systemctl start wcbroker.service")
              invocation = host.succeed("systemctl show -p InvocationID --value wcbroker.service").strip()
              host.wait_until_succeeds(f"journalctl _SYSTEMD_INVOCATION_ID={invocation} | grep -q 'held for the previous run'", timeout=30)
              assert status(k) == "retiring", status(k)
              print(release(name))
              host.wait_until_succeeds(ctl + f"status {k} | grep -q freed", timeout=30)

      with subtest("the guest is healthy, and no refused region"):
          host.fail("grep -q REFUSED /run/wcvm/console.log")
          host.succeed("systemctl is-active wcvm.service")
          print(host.succeed("cat /run/wcvm/console.log"))
          print(host.succeed("journalctl -u wcbroker --no-pager | tail -60"))
    '';
  }
