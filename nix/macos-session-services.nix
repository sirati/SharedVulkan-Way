# Assemble on vmhost, with Darwin runtime dependencies fetched from substitutes
# or cross-built beforehand. hostTools must contain a vmhost-built Mach-O opener.
{ pkgs, buildPkgs, hostTools, pulseAudio ? pkgs.pulseaudio }:
assert buildPkgs.stdenv.buildPlatform.isLinux;
assert pkgs.stdenv.hostPlatform.isDarwin;
let
  source = pkgs.lib.cleanSourceWith {
    src = ../services/macos-session;
    filter = path: _: builtins.baseNameOf path != "__pycache__";
  };
  # Use the target interpreter and pure Python library directly. A native
  # python.withPackages would add a Darwin builder for a symlink environment.
  python = pkgs.python3;
  dbusNext = pkgs.python3Packages.dbus-next;
  package = buildPkgs.runCommand "wc-macos-session-services-0.1.0" { } ''
    test -x ${hostTools}/bin/wc-portal-open
    mkdir -p $out/libexec/waycompose $out/bin $out/share/xdg-desktop-portal/portals
    cp ${source}/{bus,portal,session}.py $out/libexec/waycompose/
    cp ${source}/waycompose.portal $out/share/xdg-desktop-portal/portals/
    cp ${source}/portals.conf $out/share/xdg-desktop-portal/waycompose-portals.conf
    cat > $out/bin/wc-macos-session-services <<WRAPPER
    #!${pkgs.runtimeShell}
    export PYTHONPATH=${dbusNext}/${python.sitePackages}
    export WC_DBUS_DAEMON=${pkgs.dbus}/bin/dbus-daemon
    export WC_PULSEAUDIO=${pulseAudio}/bin/pulseaudio
    export WC_PORTAL=$out/libexec/waycompose/portal.py
    export WC_PORTAL_OPENER=${hostTools}/bin/wc-portal-open
    export WC_SESSION_PLIST=$out/Library/LaunchAgents/org.waycompose.session-services.plist
    exec ${python}/bin/python3 $out/libexec/waycompose/session.py "\$@"
    WRAPPER
    chmod +x $out/bin/wc-macos-session-services
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
in {
  inherit package;
  darwinModule = { ... }: {
    environment.systemPackages = [ package ];
    launchd.user.agents.waycompose-session-services.serviceConfig = {
      Label = "org.waycompose.session-services";
      ProgramArguments = [ "${package}/bin/wc-macos-session-services" ];
      RunAtLoad = true;
      KeepAlive = true;
      ThrottleInterval = 10;
      LimitLoadToSessionType = "Aqua";
    };
  };
}
