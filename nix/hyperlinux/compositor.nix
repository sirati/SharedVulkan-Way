# Native Darwin user service, generated on the Linux build machine.
{ pkgs, nativeBash, nativeHostTools, nativeDriverManifest, nativeVulkanLoader }:
let
  label = "org.waycompose.hyperlinux-${builtins.substring 0 12 (builtins.hashString "sha256" (toString nativeHostTools))}";
in pkgs.writeTextFile {
  name = "wc-hl-compositor";
  executable = true;
  destination = "/bin/wc-hl-compositor";
  text = ''
    #!${nativeBash}/bin/bash
    set -euo pipefail
    [[ $# == 4 && "$1" == ensure ]] || { echo 'usage: wc-hl-compositor ensure RUNTIME WAYLAND_NAME GPU_NAME' >&2; exit 2; }
    runtime=$2 wayland=$3 gpu=$4
    uid=$(id -u)
    [[ -d "$runtime" && -O "$runtime" && ! -L "$runtime" ]] || exit 1
    [[ "$wayland" != */* && "$gpu" != */* && -n "$wayland" && -n "$gpu" ]] || exit 2
    label=${label}
    domain="gui/$uid"
    if ! launchctl print "$domain/$label" >/dev/null 2>&1; then
      # Never replace a live endpoint owned by another build/session.
      [[ ! -e "$runtime/$wayland" && ! -e "$runtime/$gpu" && ! -e "$runtime/$gpu.bell" ]] || {
        echo 'A different compositor owns these endpoint names' >&2; exit 1;
      }
      plist=$(mktemp "$runtime/compositor.plist.XXXXXXXX")
      trap 'rm -f "$plist"' EXIT
      plutil -create xml1 "$plist"
      plutil -insert Label -string "$label" "$plist"
      plutil -insert ProgramArguments -json '[]' "$plist"
      args=( ${nativeHostTools}/bin/sv-guest wayland --socket "$wayland" --with-gpu hl-local \
        --runtime "$runtime" --socket "$runtime/$gpu" --bell "$runtime/$gpu.bell" )
      for i in "''${!args[@]}"; do plutil -insert "ProgramArguments.$i" -string "''${args[$i]}" "$plist"; done
      plutil -insert MachServices -json ${pkgs.lib.escapeShellArg (builtins.toJSON { "${label}.sections" = true; })} "$plist"
      plutil -insert EnvironmentVariables -json '{}' "$plist"
      plutil -insert EnvironmentVariables.WC_GPU_SECTIONS -string "$label.sections" "$plist"
      plutil -insert EnvironmentVariables.SV_VULKAN_LOADER -string ${pkgs.lib.escapeShellArg (toString nativeVulkanLoader)} "$plist"
      plutil -insert EnvironmentVariables.XDG_RUNTIME_DIR -string "$runtime" "$plist"
      plutil -insert EnvironmentVariables.VK_DRIVER_FILES -string ${pkgs.lib.escapeShellArg (toString nativeDriverManifest)} "$plist"
      plutil -insert EnvironmentVariables.VK_ICD_FILENAMES -string ${pkgs.lib.escapeShellArg (toString nativeDriverManifest)} "$plist"
      plutil -insert RunAtLoad -bool YES "$plist"
      plutil -insert StandardOutPath -string "$runtime/compositor.log" "$plist"
      plutil -insert StandardErrorPath -string "$runtime/compositor.log" "$plist"
      chmod 600 "$plist"
      launchctl bootstrap "$domain" "$plist" || launchctl print "$domain/$label" >/dev/null
    fi
    launchctl kickstart "$domain/$label"
    for ((i=0; i<300; i++)); do
      [[ -S "$runtime/$wayland" && -S "$runtime/$wayland.prime" && -S "$runtime/$gpu" && -S "$runtime/$gpu.bell" && -S "$runtime/$gpu.relay" ]] && exit 0
      sleep .1
    done
    echo 'Native Wayland/GPU service did not become ready' >&2
    exit 1
  '';
}
