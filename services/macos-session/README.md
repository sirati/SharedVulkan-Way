# Native macOS session services

`import nix/macos-session-services.nix { pkgs = darwinPkgs;
buildPkgs = hostLinuxPkgs; hostTools = hostCrossBuiltHostTools; }` returns
`package` and `darwinModule`. `buildPkgs` must target Linux, so assembly runs
on vmhost. The package contains text wrappers/configuration and references the
Darwin interpreter and compiled daemon dependencies. It does not build a
native Darwin Python environment or compile its opener on Mac.

`hostTools` is mandatory and must contain a vmhost cross-linked Mach-O
`bin/wc-portal-open`, using the existing `wc-hostplat` `MacOpener` and
`wc-hostint` URL validator, including the check against handing links back to
Linux. A Darwin cross-target cargo check is not a linked executable.
The pinned Darwin Python/dbus-next/dbus/PulseAudio compiled dependencies were
available from binary substitutes in the offline dry-run. The deployment gate
is still an actual cross-linked opener and a complete closure realized on vmhost
and copied to the target. Pass that pre-realized `sessionServices` into the
application factory. Never use target-native compilation to fill missing
compiled artifacts; missing target substitutes require a vmhost cross-build.

`nix/macos-session-package.nix` constructs both the opener and services with
`pkgs = darwinPkgs`, `buildPkgs = linuxPkgs`, a Linux Rust toolchain including
`aarch64-apple-darwin` standard libraries, and the existing private Apple SDK
passed as `sdk`. Both builders run on Linux. The opener uses the existing
clang/lld ARM64 Darwin cross-link setup and Cargo.lock; target programs are
never run during the build. The SDK remains an explicit private input, not a
repository asset or a target-machine fetch. The result also exposes
`hostTools` so its Mach-O executable and package closure can be inspected
before copying the realized service package to the target.

The module installs one Aqua LaunchAgent, `org.waycompose.session-services`.
The package also contains its plist for deployments without nix-darwin.
Installation sources do not load or change a live service. Do not install the
standalone plist as well as the module. The supervisor owns the native
`dbus-daemon`, `pulseaudio`, and portal children for its lifetime. It removes
readiness and terminates the children when any child exits or launchd stops
it. A per-user file lock prevents duplicate supervisors.

`bin/wc-macos-session-services --ensure` checks readiness and the actual portal
owner UID, then bootstraps or kickstarts this user's launchd job if needed.
It uses no forced restart and handles concurrent registrations. The command
prints the native runtime directory after readiness. Application exit must not
stop these shared user services. The normal invocation runs the supervisor.

The directory `$HOME/Library/Caches/waycompose/session-$UID` must be owned by
the native user and private. It contains:

| Path | Purpose |
| --- | --- |
| `bus` | Native D-Bus Unix socket, EXTERNAL authentication |
| `pulse/native` | Native PulseAudio protocol socket |
| `pulse/cookie` | Standard private 256-byte PulseAudio cookie |
| `ready` | Address and cookie paths, published after all children are ready |

The application factory must pass these native sockets and the cookie through
hl's file/socket bindings. D-Bus EXTERNAL must carry the actual native OS UID.
It must preserve native descriptor passing, readiness, and shared host mappings
for PulseAudio memory. The PulseAudio service retains its upstream shared
memory settings. Successful socket connection alone does not prove shared
memory audio. The cookie is standard PulseAudio interoperability, not a new
identity or user-mapping scheme. There is no audio or D-Bus transport helper.

The bus is this integration's native per-user session bus, not a system bus
or a replacement for an independently installed Homebrew session bus. Its
address must be shared by all applications in this Waycompose session. The
launchd job uses deterministic private paths rather than an unrelated
inherited session-bus address.

The portal owns `org.freedesktop.portal.Desktop` and
`org.freedesktop.impl.portal.desktop.waycompose` on that bus. It implements
actual D-Bus methods. Public Settings version 2 implements `Read`, `ReadOne`,
`ReadAll`, and `SettingChanged`. Backend Settings version 1 implements `Read`,
`ReadAll`, and `SettingChanged`. The public legacy `Read` has the required
double variant wrapping. The supported setting is
`org.freedesktop.appearance/color-scheme`, read from the native macOS
`AppleInterfaceStyle` preference. The service checks for changes every two
seconds. Accent color, contrast, and automatic-appearance behavior have not
been validated.

Public OpenURI version 1 implements `OpenURI` for `http`, `https`, and
`mailto`, with an asynchronous owned Request object. The default `ask=false`
uses the existing native default handler without an extra confirmation dialog.
`ask=true` returns NotSupported because there is no native application chooser.
Valid boolean `writable=false` is accepted; `writable=true` is unsupported since
there are no document grants. A string `activation_token` is accepted as advisory
Wayland metadata, but macOS Launch Services cannot consume that token and uses
its normal activation behavior. Parent window handles are not resolved.
Unknown extension options are ignored. Invalid standard option types fail.
`Request.Close` cancels pending native work, and caller exit or service shutdown
also cancels requests. Cancellation cannot undo a launch already accepted by
macOS. Cleanup compares task identity, so a delayed callback cannot remove a
new request that reuses the same handle token. The service checks the caller's
actual D-Bus Unix UID.

`waycompose.portal` advertises only the implemented Settings backend. The
matching `waycompose-portals.conf` selects it. On Darwin this service also
implements the public frontend, since there is no upstream Linux portal
frontend in this native session. Do not run another owner of
`org.freedesktop.portal.Desktop` on the same bus. There is no AppChooser backend
advertisement. OpenURI's default native launch is handled by this frontend.

FileChooser, document grants, ScreenCast, RemoteDesktop, screenshot,
notification, secret, inhibit, and other portal interfaces are absent. Calls
receive `UnknownMethod`, rather than a fabricated success. Native microphone
permissions and CoreAudio enumeration/playback/recording still need a Darwin
runtime test. No Linux sandbox setting or application default is changed.

## Offline validation

Run the contract test with Python containing `dbus-next` and `dbus-daemon` in
PATH. It creates a private bus and isolated HOME/XDG directories beneath
`/mnt/datasets`, injects a host stub, and never invokes a GUI or actual audio.
The test uses the actual EXTERNAL-only bus configuration from the supervisor.
Use the serialized 10 GiB build wrapper around Nix dependency realization.
The local `SessionBus` fixes dbus-next 0.2.3's socket closure and asynchronous
error context behavior. It changes only this native service runtime.

```
python3 services/macos-session/test_portal.py
```

These tests establish D-Bus method signatures, variant compatibility,
request ownership/cancellation, descriptor closure, result delivery, and refusal of unsupported
operations. They do not establish Darwin service startup, launchd behavior,
CoreAudio/TCC behavior, hl shared audio buffers, or working Linux app portals.
