{ sourceRevision, libraryPath }:
let
  flake = builtins.getFlake "git+file:///mnt/datasets/builds/shared-vulkan-base/.claude/worktrees/stock-proxy-bar";
  pkgs = import flake.inputs.nixpkgs { system = "x86_64-linux"; };
  library = builtins.path { path = builtins.toPath libraryPath; name = "sv-icd-${builtins.substring 0 8 sourceRevision}.so"; };
in pkgs.runCommand "sv-icd-driver-${builtins.substring 0 8 sourceRevision}" {} ''
  mkdir -p "$out/lib" "$out/share/vulkan/icd.d" "$out/nix-support"
  cp ${library} "$out/lib/libsv_icd.so"
  printf '{"file_format_version":"1.0.1","ICD":{"library_path":"%s/lib/libsv_icd.so","api_version":"1.4.0"}}\n' "$out" > "$out/share/vulkan/icd.d/sv_icd.json"
  echo ${sourceRevision} > "$out/nix-support/source-revision"
''
