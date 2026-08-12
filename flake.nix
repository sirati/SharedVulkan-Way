{
  description = "waycompose — a Wayland compositor that runs on Windows";

  # flake.lock is copied verbatim from shared-vulkan-base rather than generated
  # fresh. That is deliberate: the same nixpkgs revision means every store path
  # this shell needs is already realised on this machine, so `nix develop` is
  # instant and works with no network. It also means phase 1 and phase 2 are
  # built by the same rustc, which matters the moment the two link against each
  # other (the arena crate is shared eventually — see docs/design.md §6.3).
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

      # Pinned stable, same reasoning as phase 1: nothing here needs feature
      # gates, and a nightly that happens to be broken for mingw would block the
      # only target that matters.
      #
      # `x86_64-pc-windows-gnu` is the target this whole project is about. The
      # Linux target is not a nice-to-have either — it is how the protocol layer
      # gets tested against a *real* third-party Wayland client (`wayland-info`,
      # below), which does not exist on Windows and will not for a long time
      # (research §3).
      rustToolchain = pkgs.rust-bin.stable.latest.default.override {
        extensions = ["clippy" "rust-analyzer" "rust-src" "rustfmt"];
        targets = ["x86_64-pc-windows-gnu"];
      };
    in {
      devShells.default = pkgs.mkShell {
        packages = with pkgs; [
          rustToolchain

          # --- Windows cross ------------------------------------------------
          # Only the cross *compiler* belongs in a Linux dev shell — it is the
          # thing that runs here. The mingw target libraries are Windows-built
          # and nix rejects them as an unsupported system if they are named in
          # `packages`; the compiler brings its own, and the one library rustc
          # asks for by name is added as a search path in the shellHook below.
          pkgsCross.mingwW64.buildPackages.gcc

          # --- Running what we cross-compile --------------------------------
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

          # --- A real Wayland client, on the Linux side ---------------------
          # `wayland-info` is libwayland-client, written by someone else, with
          # no knowledge of this project. Running the Linux build of our server
          # against it is the only way to check the protocol layer against an
          # independent implementation, since no third-party Wayland client
          # builds for Windows at all (research §3).
          wayland-utils
          wayland

          pkg-config
          gdb
        ];

        shellHook = ''
          # cargo defaults to the gnu linker for the windows target; point it at
          # the mingw one from the store rather than whatever is on PATH.
          export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER="${pkgs.pkgsCross.mingwW64.buildPackages.gcc}/bin/x86_64-w64-mingw32-gcc"

          # rustc's windows-gnu target links `-l:libpthread.a`, which the gcc
          # wrapper does not bring along. The library lives in the mingw
          # *target* package, which cannot go in `packages` above since nix
          # rejects Windows-built libraries in a Linux shell as an unsupported
          # system. Referring to its store path is fine: we need a search path,
          # not an installation. Inherited verbatim from phase 1, where this
          # exact line was the difference between linking and not.
          export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS="-L ${pkgs.pkgsCross.mingwW64.windows.pthreads}/lib"

          # Keep every wine artefact out of the user's real ~/.wine. A prefix
          # under the build tree is disposable and cannot damage anything the
          # user cares about.
          export WINEPREFIX="''${WINEPREFIX:-/tmp/wc-wineprefix}"
          export WINEDEBUG="''${WINEDEBUG:--all}"

          echo "waycompose dev shell"
          echo "  rustc    $(rustc --version 2>/dev/null | cut -d' ' -f2)"
          echo "  targets  x86_64-unknown-linux-gnu, x86_64-pc-windows-gnu"
          echo "  wine     $(wine --version 2>/dev/null || echo 'unavailable')"
        '';
      };
    });
}
