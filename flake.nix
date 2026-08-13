{
  description = "shared_vulkan + waycompose — forward Vulkan across a VM boundary, and composite Wayland on the far side of it. One binary per side (docs/one-binary.md).";

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
      # libxkbcommon for the Windows target. smithay depends on the `xkbcommon`
      # crate non-optionally and that crate is `#[link(name = "xkbcommon")]`,
      # so without this there is no Windows *binary* — only a `cargo check`
      # that never reaches the linker. nix/libxkbcommon-mingw.nix carries the
      # measurements and the one upstream bug that had to be patched.
      #
      # `allowUnsupportedSystem` is needed because nixpkgs marks libxkbcommon
      # `meta.platforms = unix`. That marking is about the X11 and Wayland
      # halves, both of which are switched off here; the keymap engine itself
      # has upstream Windows support and builds clean.
      pkgsUnsupported = import nixpkgs {
        inherit system;
        overlays = [rust-overlay.overlays.default];
        config.allowUnsupportedSystem = true;
      };
      libxkbcommonMingw = pkgsUnsupported.callPackage ./nix/libxkbcommon-mingw.nix {};
    in {
      packages.libxkbcommon-mingw = libxkbcommonMingw;

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
          # `deqp-vk`, the Vulkan Conformance Test Suite. It is the arbiter of
          # "the functions are all implemented" — the only instrument in this
          # project that produces a number about the *API surface* rather than
          # about one workload, and the only one nobody has to take on trust.
          #
          # Pinned through the flake for the same reason `vk.xml` is: a
          # conformance number is meaningless without the version it was taken
          # against, and a CTS found on the machine is a version nobody
          # recorded. `scripts/sv-cts.sh` prints it.
          #
          # NOTE: the `vk-default` mustpass lists shipped in this derivation do
          # **not** match the binary — 362 of 1451 sampled names are absent from
          # the binary's own test hierarchy, e.g.
          # `…blit_image.all_formats.color.2d.astc_10x10_srgb_block.b8g8r8_srgb.linear_optimal_nearest`,
          # where the binary has only the `optimal_optimal_*` variants. So the
          # harness takes its caselist from `--deqp-runmode=stdout-caselist`,
          # which is the binary describing itself, and never from the lists
          # beside it. A caselist that names tests that do not exist reports
          # them as "Missing", which is neither a pass nor a skip and would
          # otherwise sit in the scoreboard as an unexplained third of it.
          vulkan-cts
          #
          # `deqp-runner` is deliberately NOT here, and it was tried first.
          # It is the standard wrapper — crash isolation, parallelism, a
          # results.csv — and 0.18.0 does not know deqp's `DeviceLost` status:
          # `Failure getting run results: unknown dEQP status 'DeviceLost'`
          # abandons the *whole group*, and every test in it is reported
          # `Missing`. On this machine that happened to four groups out of four
          # in a control run against unforwarded RADV, so the scoreboard was
          # zero passes for a driver that passes. A tool that turns a driver's
          # honest answer into an absence of data is worse here than no tool;
          # `scripts/sv-cts.sh` chunks the caselist itself, which is thirty
          # lines and knows every status deqp can print because it does not have
          # to enumerate them.
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
          # NOTE, superseded by the merge: shared_vulkan's flake said "wine is
          # deliberately NOT here", because `wineWowPackages` was not in the
          # binary cache and cost an hour of source build on shell entry.
          # waycompose's flake found that is no longer true on this machine and
          # depends on wine for its Windows-side gates, so it is in the list
          # below. The reasoning that replaces it is at `wineWow64Packages`.

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
          # Phase 1 deliberately left wine out because it was not in the binary
          # cache and cost an hour of source build on shell entry. That is no
          # longer true on this machine — it is already realised in the store —
          # and it changes what can be proven here. Without it, "it compiles for
          # Windows" is the strongest claim available locally, because the real
          # Windows guest is unreachable. With it, the handshake can be *run*.
          #
          # **It must be the staging build, and that was measured rather than
          # assumed.** `wine-11.0` has no AF_UNIX provider at all —
          # `socket(AF_UNIX, SOCK_STREAM, 0)` fails with WSAEAFNOSUPPORT
          # (10047), `WSAEnumProtocols` lists 8 providers and none with
          # `af=1`, and `grep -a AF_UNIX` over its `ws2_32.dll` finds nothing.
          # `wine-wow64-staging-11.9` lists a 9th provider,
          # `af=1 type=1 proto=0 name=AF_UNIX`, and round-trips a real
          # two-process connection. See docs/design.md §4.4 and
          # scripts/probe-socket.c.
          #
          # Read the caveat: Wine is not Windows. It is evidence that our own
          # code paths are self-consistent, not evidence about afunix.sys.
          wineWow64Packages.staging

          # --- Seeing what the Windows binary actually drew -----------------
          # Stage 3 makes windows, and "it made a window" is not a claim a log
          # line can support — the log is written by the code under test.
          #
          # These three are what make it checkable from outside. Measured, and
          # the reason this is not optional: with no `DISPLAY`, Wine's
          # `CreateWindowExW` fails for **every** visible top-level window
          # (`scripts/probe-window.c` — error 1400 or 0, while a message-only
          # window succeeds), because winex11.drv has no X server to talk to.
          # So a headless Wine cannot run this stage at all, and Xvfb is a
          # dependency rather than a testing convenience.
          #
          # `xwd` reads the root window of that server and `imagemagick`
          # converts and measures it, so the screenshot is taken by tools that
          # are not ours, from outside the process, of what X actually holds.
          # A compositor that convinced itself it had painted cannot fake this.
          # Both: `xvfb-run` is the wrapper, `xvfb` is the `Xvfb` binary it
          # needs on `PATH`. Naming only the wrapper gets a shell in which the
          # harness fails with "Xvfb: not found".
          xvfb-run
          imagemagick

          # --- A real Wayland client, on the Linux side ---------------------
          # `wayland-info` is libwayland-client, written by someone else, with
          # no knowledge of this project. Running the Linux build of our server
          # against it is the only way to check the protocol layer against an
          # independent implementation, since no third-party Wayland client
          # builds for Windows at all (research §3).
          wayland-utils
          wayland

          # ADDED FOR STAGE 21. `scripts/wc-inhibit.sh` builds a C client from
          # the XML in `crates/wc-proxy/protocols`, so that the probe's idea of
          # an opcode is the same file the compositor was generated from rather
          # than whatever `wayland-protocols` this machine happens to have. That
          # needs the code generator, which nixpkgs splits out of `wayland`
          # itself. 23 KiB.
          wayland-scanner

          # Stage 2's independent client. `wayland-info` enumerates globals and
          # stops there; reaching `xdg_toplevel` needs a client that actually
          # makes a window. Weston's demo clients are C, use libwayland and the
          # upstream `xdg-shell` protocol, and have never heard of this project
          # — which is the whole point, exactly as it was for `wayland-info` in
          # stage 1. `weston-flower` is the smallest of them that maps a
          # toplevel.
          weston

          # ADDED FOR STAGE 13. `wl-copy` and `wl-paste` are the clipboard's
          # equivalent of what `weston-flower` was for surfaces and
          # `weston-eventdemo` was for input: **libwayland clients written by
          # somebody else that drive the exact protocol under test**. `wl-copy`
          # creates a `wl_data_source`, offers mime types and calls
          # `set_selection`; `wl-paste` receives a `wl_data_offer` and calls
          # `receive` — which is the request that, before this stage, killed
          # the connection.
          #
          # `wl-paste` needs the selection, and a compositor only offers the
          # selection to the client with data-device focus, so it opens an
          # `xdg_toplevel` of its own to acquire focus and closes it again. That
          # is a real window through the real proxy, which makes the test
          # stronger rather than weaker.
          wl-clipboard

          # `xkbcli compile-keymap` — the Linux-side half of the keymap story.
          # There is no xkeyboard-config data tree on Windows and there is not
          # going to be one, so keymap *strings* are generated here, on Linux,
          # from real RMLVO names, and the Windows binary only ever compiles a
          # string it was handed. scripts/gen-keymap.sh does the generating and
          # docs/design.md §7.2 explains why this is the whole design and not a
          # test convenience.
          libxkbcommon

          # --- The conformance suite ----------------------------------------
          # WLCS, the Wayland Conformance Suite (docs/design.md §26). It is
          # C++ with a CMake build, and it is already in nixpkgs at 1.8.1 and
          # already in the binary cache — so there is no CMake invocation
          # anywhere in this tree and no vendored copy to keep in step. That is
          # worth stating because it was the part of the job that looked
          # expensive: the runner is a **binary**, `libexec/wlcs/wlcs`, which
          # dlopens a compositor integration module and runs 1,228 gtest cases
          # against whatever that module connects it to. Our module is
          # `crates/wc-wlcs`, and it is Rust, because the integration API is
          # plain C.
          #
          # It is put on PATH by the shellHook below rather than named here,
          # because nixpkgs installs the runner under `libexec` — there is no
          # `bin/wlcs` for `packages` to pick up.
          wlcs

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
          #
          # The second `-L` is libxkbcommon, built by nix/libxkbcommon-mingw.nix
          # and static, so `#[link(name = "xkbcommon")]` resolves to
          # `libxkbcommon.a` and the produced .exe carries the keymap engine
          # inside it rather than needing a DLL beside it. Both `-L`s are needed
          # now that one binary links the Vulkan side and the Wayland side.
          export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS="-L ${pkgs.pkgsCross.mingwW64.windows.pthreads}/lib -L ${libxkbcommonMingw}/lib"

          # For the C probe (scripts/probe-xkb.c) and for anything that wants to
          # find the headers without going through cargo.
          export WC_XKB_MINGW="${libxkbcommonMingw}"
          export WC_XKB_MINGW_DEV="${libxkbcommonMingw.dev}"

          # Keep every wine artefact out of the user's real ~/.wine. A prefix
          # under the build tree is disposable and cannot damage anything the
          # user cares about.
          export WINEPREFIX="''${WINEPREFIX:-/tmp/wc-wineprefix}"
          export WINEDEBUG="''${WINEDEBUG:--all}"

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

          # The WLCS runner and its headers. `libexec` rather than `bin` is
          # nixpkgs' own layout for it, and the header path is what a future C
          # or bindgen consumer would want; `scripts/wc-wlcs.sh` reads both.
          export WC_WLCS_RUNNER="${pkgs.wlcs}/libexec/wlcs/wlcs"
          export WC_WLCS_INCLUDE="${pkgs.wlcs}/include"

          echo "shared_vulkan + waycompose dev shell — one binary per side"
          echo "  rustc    $(rustc --version 2>/dev/null | cut -d' ' -f2)"
          echo "  targets  x86_64-unknown-linux-gnu, x86_64-pc-windows-gnu, x86_64-unknown-linux-musl"
          echo "  vk.xml   $VK_XML"
          echo "  wlcs     ${pkgs.wlcs.version}"
          echo "  wine     $(wine --version 2>/dev/null || echo 'unavailable')"
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
