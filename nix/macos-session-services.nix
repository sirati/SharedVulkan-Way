# Import with a Darwin pkgs set. An existing native hostTools package can
# supply bin/wc-portal-open to avoid building it twice. Linux evaluation can use
# allowUnsupportedSystem for the source package, never to claim a Darwin run.
{ pkgs, hostTools ? null }:
let
  nativeOpener = pkgs.rustPlatform.buildRustPackage {
    pname = "wc-portal-open";
    version = "0.1.0";
    src = pkgs.lib.fileset.toSource {
      root = ../.;
      fileset = pkgs.lib.fileset.unions [ ../Cargo.toml ../Cargo.lock ../crates ../vendor ];
    };
    cargoLock.lockFile = ../Cargo.lock;
    cargoBuildFlags = [ "-p" "wc-hostplat" "--bin" "wc-portal-open" ];
    doCheck = false;
    meta.platforms = pkgs.lib.platforms.darwin;
  };
  opener = if hostTools == null then nativeOpener else hostTools;
  python = pkgs.python3.withPackages (p: [ p.dbus-next ]);
  package = pkgs.stdenvNoCC.mkDerivation {
    pname = "wc-macos-session-services";
    version = "0.1.0";
    src = pkgs.lib.cleanSourceWith {
      src = ../services/macos-session;
      filter = path: _: builtins.baseNameOf path != "__pycache__";
    };
    nativeBuildInputs = [ pkgs.makeWrapper ];
    installPhase = ''
      mkdir -p $out/libexec/waycompose $out/bin $out/share/xdg-desktop-portal/portals
      cp bus.py portal.py session.py $out/libexec/waycompose/
      cp waycompose.portal $out/share/xdg-desktop-portal/portals/
      cp portals.conf $out/share/xdg-desktop-portal/waycompose-portals.conf
      makeWrapper ${python}/bin/python $out/bin/wc-macos-session-services \
        --add-flags $out/libexec/waycompose/session.py \
        --set WC_DBUS_DAEMON ${pkgs.dbus}/bin/dbus-daemon \
        --set WC_PULSEAUDIO ${pkgs.pulseaudio}/bin/pulseaudio \
        --set WC_PORTAL $out/libexec/waycompose/portal.py \
        --set WC_PORTAL_OPENER ${opener}/bin/wc-portal-open \
        --set WC_SESSION_PLIST $out/Library/LaunchAgents/org.waycompose.session-services.plist
      mkdir -p $out/Library/LaunchAgents
      cat > $out/Library/LaunchAgents/org.waycompose.session-services.plist <<PLIST
      <?xml version="1.0" encoding="UTF-8"?>
      <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
      <plist version="1.0"><dict>
        <key>Label</key><string>org.waycompose.session-services</string>
        <key>ProgramArguments</key><array><string>$out/bin/wc-macos-session-services</string></array>
        <key>RunAtLoad</key><true/>
        <key>KeepAlive</key><true/>
        <key>ThrottleInterval</key><integer>10</integer>
        <key>LimitLoadToSessionType</key><string>Aqua</string>
      </dict></plist>
      PLIST
    '';
    meta.platforms = pkgs.lib.platforms.darwin;
  };
in {
  inherit package nativeOpener;
  # Import in nix-darwin. This changes no live service until the user deploys.
  darwinModule = { ... }: {
    environment.systemPackages = [ package ];
    launchd.user.agents.waycompose-session-services = {
      serviceConfig = {
        Label = "org.waycompose.session-services";
        ProgramArguments = [ "${package}/bin/wc-macos-session-services" ];
        RunAtLoad = true;
        KeepAlive = true;
        ThrottleInterval = 10;
        LimitLoadToSessionType = "Aqua";
      };
    };
  };
}
