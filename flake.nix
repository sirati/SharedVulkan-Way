{
  description = "shared_vulkan — forward Vulkan across a VM boundary, in either direction";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-compat = {
      url = "github:edolstra/flake-compat";
      flake = false;
    };
  };

  outputs = {
    nixpkgs,
    flake-utils,
    rust-overlay,
    ...
  }:
    flake-utils.lib.eachSystem ["x86_64-linux"] (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [rust-overlay.overlays.default];
      };

      # Toolchain from the nix store, pinned by flake.lock. No rustup, no
      # ~/.rustup state, nothing fetched when the shell is entered.
      #
      # Stable rather than nightly: nothing here is expected to need feature
      # gates, and this project has to build for a Windows target as well,
      # where a nightly that happens to be broken for mingw would block both
      # halves at once.
      rustToolchain = pkgs.rust-bin.stable.latest.default.override {
        extensions = ["clippy" "rust-analyzer" "rust-src" "rustfmt"];
        # The client and the server each have to build for the opposite OS
        # from the one they run on, because either end may be Linux or
        # Windows. Cross-compiling both from Linux keeps one build machine.
        # musl is here for one reason, and it is not portability in the abstract:
        # `arena-host` and `sv-hostd` run on **testhost**, whose nix store is
        # not this machine's. A glibc binary built here names an interpreter
        # under a store path that does not exist there, and the only symptom is
        # `cannot execute: required file not found` — which reads like a missing
        # binary rather than a missing loader, and cost a run of the GPU
        # experiment to diagnose. A statically linked musl build has no
        # interpreter and no store path in it at all, so it runs wherever it is
        # copied. It is also what makes the app side deployable to the target at
        # all without putting a Rust toolchain on somebody's daily laptop.
        targets = ["x86_64-pc-windows-gnu" "x86_64-unknown-linux-musl"];
      };
      # Miri needs nightly, and nothing else here does. Keeping it in a separate
      # shell means the default shell stays on the pinned stable toolchain —
      # which is the whole point of the comment above — while
      # `design.md` §7.3's "every generated round-trip under Miri" is still
      # something a person can run rather than something a document claims.
      miriToolchain = pkgs.rust-bin.selectLatestNightlyWith (toolchain:
        toolchain.default.override {
          extensions = ["miri" "rust-src"];
        });
    in {
      devShells.miri = pkgs.mkShell {
        packages = [miriToolchain pkgs.vulkan-headers];
        shellHook = ''
          export VK_XML="${pkgs.vulkan-headers}/share/vulkan/registry/vk.xml"
          # Miri interprets; the generated protocol is large and its round-trip
          # tests are many, so give it a target directory of its own rather than
          # invalidating the ordinary one on every switch.
          export CARGO_TARGET_DIR="$PWD/target/miri"
          echo "shared_vulkan miri shell — cargo miri test -p sv-wire -p sv-proto"
        '';
      };

      devShells.default = pkgs.mkShell {
        packages = with pkgs; [
          rustToolchain

          # --- Vulkan ---------------------------------------------------
          # vulkan-headers carries registry/vk.xml, which is the input to the
          # protocol codegen: the API is far too large to marshal by hand, so
          # the wire format is generated from the registry rather than typed
          # out. Pinning it through nix means the generated protocol moves
          # only when we deliberately bump the flake.
          vulkan-headers
          vulkan-loader
          # vulkaninfo, for establishing what a real implementation reports
          # versus what ours claims.
          #
          # KNOWN, measured 2026-08-13: the RTX 5090 does NOT enumerate inside
          # this shell, while `nix shell nixpkgs#vulkan-tools` on the same
          # machine lists all three devices. The loader reports "Failed loading
          # library associated with ICD JSON" for nvidia_icd.json — yet a plain
          # dlopen of that same library FROM THIS SHELL succeeds, so it is not
          # a search-path fault, and prepending /run/opengl-driver/lib does not
          # fix it. Cause unknown.
          #
          # It matters more than a missing device: an NVIDIA measurement taken
          # in here is taken on a machine where NVIDIA is absent, and the
          # absence looks like a driver that enumerates nothing. Take
          # NVIDIA-specific measurements in a clean `nix shell` until this is
          # understood.
          vulkan-tools
          # The forwarded implementation has to survive validation, not just
          # run one triangle. Zink, DXVK and vkd3d-proton all lean on
          # behaviour that a sloppy forwarder gets away with until it does
          # not.
          vulkan-validation-layers
          # glslangValidator, so a compute shader can be written as GLSL and
          # compiled to SPIR-V by the build rather than checked in as an opaque
          # blob nobody can read or re-derive. `gpu-probe`'s build.rs needs it.
          glslang

          # --- Windows cross ---------------------------------------------
          # Only the cross *compiler* belongs in a Linux dev shell — it is the
          # thing that runs here. The mingw target libraries it links against
          # are Windows-built and nix rejects them as an unsupported system if
          # they are named directly; the compiler brings its own.
          pkgsCross.mingwW64.buildPackages.gcc
          # NOTE: wine is deliberately NOT here. Running the Windows-side
          # binaries locally would be convenient, but `wineWowPackages` is not
          # in the binary cache and builds wine from source on shell entry,
          # which costs about an hour before anyone can type a command. Add it
          # in a separate optional shell if and when it is actually needed.

          # --- OpenGL, through Zink ----------------------------------------
          # The stated acceptance criterion of this stage is an OpenGL program
          # running on Zink on top of our forwarded Vulkan. Three things are
          # needed for that and none of them were in this shell:
          #
          #   mesa      — carries `zink_dri.so`, the Gallium driver that turns GL
          #               into Vulkan, plus libgbm and libEGL. The system has a
          #               mesa at /run/opengl-driver, but that is a NixOS
          #               artefact of *this* machine and the shell should not
          #               depend on the host's graphics stack being present.
          #   libglvnd  — the GL/EGL/GLX dispatch libraries a GL program links
          #               against; mesa is loaded *through* it.
          #   mesa-demos— glxinfo, glxgears, eglinfo, es2gears. The smallest real
          #               GL applications there are, and the ones §13's
          #               acceptance criterion names.
          #
          # `xvfb` is here because glxinfo and glxgears are GLX programs and
          # GLX needs an X server. This machine is headless (no DISPLAY), so the
          # only way to run the two programs the brief actually names is a
          # virtual framebuffer. `xwd` captures what was drawn, so
          # "it rendered" can be a checked pixel rather than a claim.
          mesa
          libglvnd
          mesa-demos
          xvfb
          xwd
          xdpyinfo
          netpbm # xwdtopnm, so a captured frame can be inspected without a GUI

          # --- Video -------------------------------------------------------
          # `video-probe` decodes H.264 through VK_KHR_video_decode_queue, and
          # both halves of judging that need ffmpeg: it *makes* the fixture —
          # an all-IDR elementary stream, which no other tool here can produce —
          # and it *decodes the same stream in software*, which is the control
          # the Vulkan output is compared against. A hardware decoder that
          # agrees byte-for-byte with libavcodec is a much stronger statement
          # than one whose picture looks plausible, and it is the same argument
          # as comparing zink against llvmpipe.
          #
          # It is the "small" ffmpeg, so there is no CUDA/AMF in the closure.
          ffmpeg

          # --- Misc --------------------------------------------------------
          pkg-config
          # Reading the shared-memory region while debugging both ends.
          gdb
        ];

        shellHook = ''
          # Where the codegen reads the registry from, so it is never a
          # question of which vk.xml happened to be on the machine.
          export VK_XML="${pkgs.vulkan-headers}/share/vulkan/registry/vk.xml"
          export VULKAN_SDK="${pkgs.vulkan-headers}"

          # The ICD's end-to-end test drives the *real* loader, which means
          # `dlopen("libvulkan.so.1")` from a test binary. `vulkaninfo` finds it
          # through its own rpath and a test binary has none, so without this the
          # one test that exercises the loader contract skips silently — which is
          # the worst possible outcome for a test whose whole point is that the
          # loader accepts us.
          export LD_LIBRARY_PATH="${pkgs.vulkan-loader}/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

          # cargo defaults to the gnu linker for the windows target; point it
          # at the mingw one from the store rather than whatever is on PATH.
          export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="${pkgs.pkgsCross.mingwW64.buildPackages.gcc}/bin/x86_64-w64-mingw32-gcc"

          # rustc's windows-gnu target links `-l:libpthread.a`, which the gcc
          # wrapper does not bring along. The library lives in the mingw
          # *target* package — which cannot go in `packages` above, since nix
          # rejects Windows-built libraries in a Linux shell as an unsupported
          # system. Referring to its store path is fine, though: we only need a
          # search path, not to install it.
          export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS="-L ${pkgs.pkgsCross.mingwW64.windows.pthreads}/lib"

          # Where Zink lives. mesa's Gallium drivers are `dlopen`ed from a
          # directory the loader finds by convention, and on a non-NixOS-graphics
          # path (or inside a bubblewrap container that has no
          # /run/opengl-driver) it has to be named. Pointing at the *shell's*
          # mesa rather than the system one is deliberate: it makes the GL half
          # of this project reproducible from flake.lock rather than dependent on
          # whatever the host happens to have configured.
          export SV_MESA="${pkgs.mesa}"
          export LIBGL_DRIVERS_PATH="${pkgs.mesa}/lib/dri"
          export __EGL_VENDOR_LIBRARY_DIRS="${pkgs.mesa}/share/glvnd/egl_vendor.d"

          echo "shared_vulkan dev shell"
          echo "  rustc    $(rustc --version 2>/dev/null | cut -d' ' -f2)"
          echo "  vk.xml   $VK_XML"
        '';
      };

      # The development guest of §13 step 7.
      #
      # `nix build .#svGuest` produces a script that boots a NixOS VM whose RAM
      # is a `memory-backend-memfd` the host can map, with a QMP socket and a
      # virtio-serial cold path. That is the whole cross-VM topology on a
      # machine that can be rebooted, as against `testhost`, which cannot.
      #
      # The store is shared over 9p rather than copied into an image, so the
      # guest can run a binary that was `cargo build`ed a second ago.
      packages.svGuest =
        (nixpkgs.lib.nixosSystem {
          inherit system;
          modules = [
            ./nix/guest.nix
            "${nixpkgs}/nixos/modules/virtualisation/qemu-vm.nix"
          ];
        })
        .config
        .system
        .build
        .vm;
    });
}
