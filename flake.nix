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
        targets = ["x86_64-pc-windows-gnu"];
      };
    in {
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
          vulkan-tools
          # The forwarded implementation has to survive validation, not just
          # run one triangle. Zink, DXVK and vkd3d-proton all lean on
          # behaviour that a sloppy forwarder gets away with until it does
          # not.
          vulkan-validation-layers

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

          echo "shared_vulkan dev shell"
          echo "  rustc    $(rustc --version 2>/dev/null | cut -d' ' -f2)"
          echo "  vk.xml   $VK_XML"
        '';
      };
    });
}
