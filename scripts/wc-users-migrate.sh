#!/usr/bin/env bash
#
# §111.10: move a VM host's waycompose installations from the admin's user
# units to the system services (`services.waycompose` in the host's NixOS
# configuration), with each installation's sessions run as the Linux user its
# guest user is mapped to. Run as root on the VM host, after switching to the
# NixOS generation that has `services.waycompose` (whose services wait, by
# ConditionPathExists, for the binaries this script installs).
#
#   wc-users-migrate.sh plan     ADMIN DESKTOP BIN-DIR INSTALL...   say every step, change nothing
#   wc-users-migrate.sh apply    ADMIN DESKTOP BIN-DIR INSTALL...   do it; the admin's copies stay
#   wc-users-migrate.sh verify   ADMIN DESKTOP BIN-DIR INSTALL...   check the new layout, as DESKTOP
#   wc-users-migrate.sh remove-old ADMIN DESKTOP BIN-DIR INSTALL... delete the admin's copies (after a
#                                                                     session was seen working)
#   wc-users-migrate.sh rollback ADMIN DESKTOP BIN-DIR INSTALL...   restore a protected system-layout installation snapshot
#   wc-users-migrate.sh install-staged ADMIN auto STAGE NAME          (wc-launch.sh deploy, system layout:
#                                                                     one staged installation in; its user
#                                                                     from users.conf)
#
# ADMIN is the account whose user units ran the hub and the session service
# (<admin-user>), DESKTOP the Linux user the guest user is mapped to
# (<desktop-user>), BIN-DIR a directory with this branch's builds
# (wc-arena-host, sv-hostd, wc-proxy, wc-launch, wc-host-open, libsv_icd.so,
# ...: every file of an installation's bin/ found there is replaced) and
# scripts/wc-session. INSTALL is an installation name registered in
# ~ADMIN/.config/waycompose/installs/.
#
# What moves where (docs/waycompose-design.md §111.10):
#
#   ~ADMIN/.config/waycompose/installs/NAME -> BASE   /var/lib/waycompose/installs/NAME
#     (wc-session, apps.ini, gpu.env, bin/; root's, 0755; session.key the
#     service account's, 0600; GC roots in bin/ registered again)
#   paths under ~ADMIN that apps.ini names           ~DESKTOP/<same path>, DESKTOP's,
#     (~/.wc-launch/<profile>, ~/wc-mc, ...)          absolute paths in their text files
#                                                     rewritten; apps.ini says ~/...
#   ~ADMIN/.local/state/waycompose/sessiond/NAME.seq  /var/lib/waycompose/sessiond/
#   the admin's wc-bell-hub and wc-sessiond units     stopped, disabled, kept in the backup
#   ACLs of ADMIN and DESKTOP on /run/windows-vm/wc-arena/*   removed (the running VM's;
#                                                     its next start makes none)
#
# Nothing of the VM is restarted. The hub's restart drops QEMU's two chardev
# connections for about a second; the guest rescans its rings on reconnect
# (§99). Run it while no session runs (it refuses otherwise, unless FORCE=1).
#
# For a dry run on another machine: ROOT=/some/dir prefixes every path, and
# without root nothing is chowned and no unit is touched (said per step).
set -euo pipefail

VERB=${1:-}; ADMIN=${2:-}; DESK=${3:-}; BINS=${4:-}
shift 4 2>/dev/null || { sed -n '3,40p' "$0"; exit 2; }
INSTALLS=("$@")
[ ${#INSTALLS[@]} -gt 0 ] || { echo "no installation named"; exit 2; }
case "$VERB" in plan|apply|verify|remove-old|rollback|install-staged|key-prepare|key-export|key-confirm|services-restart) ;; *) echo "unknown verb $VERB"; exit 2 ;; esac

ROOT=${ROOT:-}
REAL=1; [ "$(id -u)" = 0 ] && [ -z "$ROOT" ] || REAL=""
homedir() { if [ -n "$ROOT" ]; then echo "$ROOT/home/$1"; else getent passwd "$1" | cut -d: -f6; fi; }
# install-staged: DESKTOP `auto` is the Linux user users.conf maps the
# installation to (the first line that names it, or names no installs).
if [ "$DESK" = auto ]; then
  DESK=$(awk -v n="${INSTALLS[0]}" '/^#/||NF<3{next} {ok=1; for(i=4;i<=NF;i++) if ($i ~ /^installs=/) { ok=0; k=split(substr($i,10),a,","); for(j=1;j<=k;j++) if (a[j]==n) ok=1 } if (ok) {print $3; exit} }' "$ROOT/etc/waycompose/users.conf" 2>/dev/null)
  [ -n "$DESK" ] || { echo "users.conf maps no user for ${INSTALLS[0]}"; exit 1; }
fi
AH=$(homedir "$ADMIN"); DH=$(homedir "$DESK")
# What the files say: the real homes, even in a dry run under ROOT.
AH_REAL=${AH#"$ROOT"}; DH_REAL=${DH#"$ROOT"}
VL="$ROOT/var/lib/waycompose"
ARENA_DIR="$ROOT/run/windows-vm/wc-arena"
BACKUP="$VL/migration-backup-$ADMIN"
DRY=""; [ "$VERB" = plan ] && DRY=1
FAIL=0

say() { echo "[$VERB] $*"; }
run() { # a step: printed, and done unless planning
  say "  \$ $*"
  [ -n "$DRY" ] && return 0
  "$@" || { echo "FAILED: $*"; FAIL=1; return 1; }
}
own() { # USER[:GROUP] PATH... (recursive)
  local who=$1; shift
  if [ -n "$REAL" ]; then run chown -R "$who" "$@"; else say "  (would chown -R $who $*)"; fi
}
as_user() { # USER CMD...: a systemctl --user of USER
  local u=$1; shift
  if [ -n "$REAL" ]; then run systemctl --machine="$u@.host" --user "$@"; else say "  (would: systemctl --user -M $u@ $*)"; fi
}
sys() { if [ -z "$DRY" ] && { [ "$1" = restart ] || [ "$1" = start ]; }; then require_all_trust; fi; if [ -n "$REAL" ]; then run systemctl "$@"; else say "  (would: systemctl $*)"; fi; }

[ -d "$AH" ] || { echo "no home for $ADMIN ($AH)"; exit 1; }
[ -d "$DH" ] || { echo "no home for $DESK ($DH)"; exit 1; }
DGROUP=$(id -gn "$DESK" 2>/dev/null || echo "$DESK")

# The paths under the admin's home an installation's apps.ini names, as the
# unit to move: ~/.wc-launch/<profile>, else the first directory under ~.
user_data() { # APPS.INI BASE
  grep -oE "$AH_REAL/[^ ;:\"']+" "$1" 2>/dev/null | while read -r p; do
    case "$p" in "$2"/*|"$2") continue ;; esac
    r=${p#"$AH_REAL"/}
    case "$r" in
      .wc-launch/*) r=${r#.wc-launch/}; echo ".wc-launch/${r%%/*}" ;;
      .config/*|.local/*|.cache/*) echo "UNSUPPORTED:$p" ;;
      *) echo "${r%%/*}" ;;
    esac
  done | sort -u
}

base_of() { readlink -f "$AH/.config/waycompose/installs/$1" 2>/dev/null; }

# Trust is recorded outside admin-writable staging trees. A key first seen
# in session.key has no privileged provenance, regardless of its current mode.
for n in "${INSTALLS[@]}"; do
  [[ "$n" =~ ^[a-zA-Z0-9_-]+$ ]] || { echo "invalid installation name"; exit 1; }
done
TRUST="$VL/key-trust"
secure_path() {
  local uid=0; [ -z "$ROOT" ] || uid=$(id -u)
  [ ! -L "$1" ] && [ "$(stat -c '%u %a' "$1" 2>/dev/null)" = "$uid $2" ]
}
generated_candidate() {
  local d="$TRUST/$1"
  secure_path "$VL" 755 && secure_path "$TRUST" 700 && secure_path "$d" 700 &&
    secure_path "$d/session.key" 600 && secure_path "$d/generated-here" 600 &&
    [ "$(cat "$d/generated-here")" = 'root-generated-candidate-v1' ] &&
    grep -Eq '^[0-9a-f]{64}$' "$d/session.key"
}
trusted_key() {
  local d="$TRUST/$1"
  generated_candidate "$1" && secure_path "$d/broker-confirmed" 600 &&
    [ "$(cat "$d/broker-confirmed")" = 'root-generated-broker-confirmed-v1' ] &&
    grep -Eq '^[0-9a-f]{64}$' "$d/session.key"
}
require_trust() {
  trusted_key "$1" || { echo "installation $1 has no protected broker-confirmed key; provision it through a system-layout broker key deploy first"; exit 1; }
}
require_all_trust() {
  local path name found=0
  for path in "$VL"/installs/*; do
    [ -d "$path" ] || continue
    name=${path##*/}
    [[ "$name" =~ ^[a-zA-Z0-9_-]+$ ]] && [ ! -L "$path" ] || { echo "untrusted installed directory"; exit 1; }
    require_trust "$name"
    cmp -s "$path/session.key" "$TRUST/$name/session.key" || { echo "installation $name still has a retired verifier; services stay stopped"; exit 1; }
    found=1
  done
  [ "$found" = 1 ] || { echo "no protected installations to serve"; exit 1; }
}
require_idle() {
  [ -n "$REAL" ] || return 0
  local u active
  for u in "$ADMIN" "$DESK"; do
    active=$(systemctl --machine="$u@.host" --user list-units --state=active --no-legend --plain 'wc-launch-*') || exit 1
    [ -z "$active" ] || { echo "active sessions must end before protected key or installation changes"; exit 1; }
  done
}
legacy_snapshot() {
  local n=$1 base=$2 base_real=$3 nb_real=$4
  local dest="$VL/protected-rollback/$n.preparing"
  # Preserve the known application installation before candidate binaries or
  # scripts replace it. Keys and paths are converted to the protected layout.
  run install -d -m 0700 "$VL/protected-rollback"
  [ ! -e "$dest" ] && [ ! -L "$dest" ] || { echo "unfinished application snapshot requires review"; exit 1; }
  run cp -a "$base" "$dest"
  run rm -f "$dest/session.key"
  run cp "$TRUST/$n/session.key" "$dest/session.key"
  own root:root "$dest"
  run chmod -R go-w "$dest"
  if [ -n "$REAL" ]; then run chown waycompose:waycompose "$dest/session.key"; fi
  run chmod 0600 "$dest/session.key"
  run sed -i -e "s|$base_real/|$nb_real/|g" -e "s|$base_real\$|$nb_real|g" -e "s|$AH_REAL/|~/|g" "$dest/apps.ini"
  for f in gpu.env copy-scope dmabuf-node; do
    [ -f "$dest/$f" ] && run sed -i "s|$base_real/|$nb_real/|g" "$dest/$f"
  done
  run rm -rf "$VL/protected-rollback/$n"
  run mv "$dest" "$VL/protected-rollback/$n"
}
initial_snapshot() {
  local n=$1
  if [ ! -e "$VL/protected-rollback/$n" ]; then
    run install -d -m 0700 "$VL/protected-rollback"
    run cp -a "$VL/installs/$n" "$VL/protected-rollback/$n"
  fi
}
if [ "$VERB" = services-restart ]; then
  sys restart wc-bell-hub.service wc-sessiond.service
  exit 0
fi
if [[ "$VERB" = key-* ]]; then
  n=${INSTALLS[0]}; d="$TRUST/$n"
  [ -n "$REAL" ] || [ -n "$ROOT" ] || { echo "key management requires root"; exit 1; }
  if [ "$VERB" = key-prepare ]; then
    # Close both verifier entry points before replacing an exposed key. They
    # stay stopped on failure. Retrying uses the same protected candidate.
    if ! trusted_key "$n" || ! cmp -s "$VL/installs/$n/session.key" "$d/session.key"; then
      if [ -n "$REAL" ]; then
        require_idle
        systemctl --machine="$ADMIN@.host" --user stop wc-sessiond.service wc-bell-hub.service
        systemctl --machine="$ADMIN@.host" --user disable wc-sessiond.service wc-bell-hub.service
        systemctl stop wc-sessiond.service wc-bell-hub.service
      fi
      install -d -m 0755 "$VL"
      secure_path "$VL" 755 || { echo "untrusted service directory"; exit 1; }
      for dir in "$TRUST" "$d"; do
        [ ! -e "$dir" ] && [ ! -L "$dir" ] && install -d -m 0700 "$dir"
        secure_path "$dir" 700 || { echo "untrusted key directory"; exit 1; }
      done
      [ ! -L "$d/rotation-pending" ] || exit 1
      (umask 077; printf '%s\n' protected-rotation-pending-v1 >"$d/rotation-pending")
      # Deleting the retired verifier prevents a reboot or service restart
      # from accepting exposed trust while deployment is incomplete.
      rm -f "$VL/installs/$n/session.key"
      legacy=$(base_of "$n") || legacy=""
      case "$legacy" in "$AH"/*) rm -f "$legacy/session.key" ;; esac
      if [ ! -e "$d/session.key" ]; then
        (umask 077; head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' >"$d/session.key")
        (umask 077; printf '%s\n' root-generated-candidate-v1 >"$d/generated-here")
      fi
      generated_candidate "$n" || { echo "unknown candidate provenance; refusing to adopt a preexisting key"; exit 1; }
    fi
  elif [ "$VERB" = key-export ]; then
    generated_candidate "$n" || { echo "unknown candidate provenance"; exit 1; }
    grep -Eq '^[0-9a-f]{64}$' "$d/session.key" || exit 1
    cat "$d/session.key"
    exit 0
  else
    generated_candidate "$n" || { echo "unknown candidate provenance"; exit 1; }
    # Only called after the broker's privileged stdin provisioning succeeded.
    [ ! -L "$d/broker-confirmed" ] || exit 1
    (umask 077; printf '%s\n' root-generated-broker-confirmed-v1 >"$d/broker-confirmed")
  fi
  exit 0
fi
if [ "$VERB" = apply ] || [ "$VERB" = install-staged ] || [ "$VERB" = rollback ]; then
  for n in "${INSTALLS[@]}"; do require_trust "$n"; done
fi

# ------------------------------------------------------------------- checks
if [ "$VERB" != rollback ] && [ "$VERB" != verify ] && [ "$VERB" != remove-old ] && [ "$VERB" != install-staged ]; then
  for f in wc-arena-host sv-hostd wc-session; do
    [ -f "$BINS/$f" ] || { echo "no $BINS/$f: BIN-DIR must hold this branch's builds"; exit 1; }
  done
  if [ -n "$REAL" ]; then
    getent passwd waycompose >/dev/null || { echo "no account waycompose: switch to the generation with services.waycompose first"; exit 1; }
    [ -s /etc/waycompose/users.conf ] || { echo "no /etc/waycompose/users.conf"; exit 1; }
    live=$(systemctl --machine="$ADMIN@.host" --user list-units --state=active --no-legend --plain 'wc-launch-*' 2>/dev/null | wc -l)
    if [ "$live" != 0 ] && [ "${FORCE:-}" != 1 ]; then
      echo "$live waycompose session unit(s) of $ADMIN are running; end them (close the windows) or FORCE=1"; exit 1
    fi
  fi
fi

# -------------------------------------------------------------------- apply
if [ "$VERB" = plan ] || [ "$VERB" = apply ]; then
  say "1. the admin's services stop; their units go to $BACKUP"
  run mkdir -p "$BACKUP/units" "$BACKUP/links" "$BACKUP/state"
  as_user "$ADMIN" stop wc-sessiond.service wc-bell-hub.service
  as_user "$ADMIN" disable wc-sessiond.service wc-bell-hub.service
  for u in wc-sessiond.service wc-bell-hub.service; do
    [ -e "$AH/.config/systemd/user/$u" ] && run mv "$AH/.config/systemd/user/$u" "$BACKUP/units/"
  done

  say "2. the services' binaries: $VL/bin"
  run install -d -m 0755 "$VL/bin" "$VL/installs"
  run install -m 0755 "$BINS/wc-arena-host" "$BINS/sv-hostd" "$VL/bin/"

  for n in "${INSTALLS[@]}"; do
    base=$(base_of "$n")
    [ -n "$base" ] && [ -f "$base/wc-session" ] || { echo "installation $n: no ~$ADMIN/.config/waycompose/installs/$n with a wc-session"; FAIL=1; continue; }
    nb="$VL/installs/$n"; nb_real=${nb#"$ROOT"}; base_real=${base#"$ROOT"}
    say "3. installation $n: $base -> $nb_real"
    if [ -e "$nb" ]; then
      cmp -s "$nb/session.key" "$TRUST/$n/session.key" ||
        { secure_path "$TRUST/$n/rotation-pending" 600 && [ ! -e "$nb/session.key" ]; } ||
        { echo "existing installation lacks the current trusted verifier"; exit 1; }
    fi
    data=$(user_data "$base/apps.ini" "$base_real")
    if echo "$data" | grep -q '^UNSUPPORTED:'; then
      echo "  apps.ini names $(echo "$data" | grep '^UNSUPPORTED:' | cut -d: -f2-): move it by hand"; FAIL=1; continue
    fi
    legacy_snapshot "$n" "$base" "$base_real" "$nb_real"
    if [ ! -e "$nb" ]; then run cp -a "$base" "$nb"; fi
    run rm -f "$nb/session.key"
    run cp -f "$TRUST/$n/session.key" "$nb/session.key"
    # This branch's builds over every file of bin/ they name; links stay.
    for f in "$base"/bin/*; do
      b=$(basename "$f")
      [ -f "$BINS/$b" ] && [ ! -L "$f" ] && run install -m 0755 "$BINS/$b" "$nb/bin/$b"
    done
    run install -m 0755 "$BINS/wc-session" "$nb/wc-session"
    # The definitions are root's; the key the service's alone.
    own root:root "$nb"
    run chmod -R go-w "$nb"
    if [ -n "$REAL" ]; then run chown waycompose:waycompose "$nb/session.key"; else say "  (would chown waycompose $nb/session.key)"; fi
    run chmod 0600 "$nb/session.key"
    # GC roots made by deploy for bin/'s store links point at the old path.
    for l in "$nb"/bin/*; do
      [ -L "$l" ] || continue
      t=$(readlink "$l"); case "$t" in /nix/store/*) ;; *) continue ;; esac
      if [ -n "$REAL" ]; then run nix-store --add-root "$l" --indirect -r "$t" >/dev/null; else say "  (would register GC root $l -> $t)"; fi
    done
    # apps.ini: the base's paths are the new base's; the admin's home is ~/.
    run sed -i -e "s|$base_real/|$nb_real/|g" -e "s|$base_real\$|$nb_real|g" -e "s|$AH_REAL/|~/|g" "$nb/apps.ini"
    for f in gpu.env copy-scope dmabuf-node; do
      [ -f "$nb/$f" ] && run sed -i "s|$base_real/|$nb_real/|g" "$nb/$f"
      [ -f "$nb/$f" ] && grep -q "$AH_REAL/" "$nb/$f" && { echo "  $nb/$f still names $AH_REAL: fix by hand"; FAIL=1; }
    done
    for u in $data; do
      src="$AH/$u"; dst="$DH/$u"
      [ -e "$src" ] || { say "  (apps.ini names ~/$u, which $ADMIN does not have: nothing to move)"; continue; }
      say "4. $n's data ~$ADMIN/$u -> ~$DESK/$u"
      [ -e "$dst" ] && { echo "  $dst exists; refusing to merge"; FAIL=1; continue; }
      run mkdir -p "$(dirname "$dst")"
      run cp -a "$src" "$dst"
      own "$DESK:$DGROUP" "$dst"
      [ "$(dirname "$u")" != . ] && own "$DESK:$DGROUP" "$DH/$(dirname "$u")"
      # Absolute paths inside it (Prism's configs, recorded launches, the
      # wrapper, Firefox's prefs) name the new home.
      if [ -z "$DRY" ]; then
        grep -rlIZ "$AH_REAL/$u" "$dst" 2>/dev/null | xargs -0 -r sed -i "s|$AH_REAL/$u|$DH_REAL/$u|g"
      else
        say "  (would rewrite $AH_REAL/$u -> $DH_REAL/$u in $(grep -rlI "$AH_REAL/$u" "$src" 2>/dev/null | wc -l) text file(s))"
      fi
      # A profile copied while nothing ran: no lock to inherit.
      run rm -f "$dst/lock" "$dst/.parentlock"
    done
    st="$AH/.local/state/waycompose/sessiond/$n.seq"
    if [ -f "$st" ]; then
      run install -d -m 0700 "$VL/sessiond"
      run cp "$st" "$VL/sessiond/$n.seq"
      if [ -n "$REAL" ]; then run chown -R waycompose:waycompose "$VL/sessiond"; fi
    fi
    run mv "$AH/.config/waycompose/installs/$n" "$BACKUP/links/$n"
  done

  say "5. the running VM's ACLs: only the service account by name"
  for f in "$ARENA_DIR" "$ARENA_DIR"/arena "$ARENA_DIR"/gpu "$ARENA_DIR"/*.sock; do
    [ -e "$f" ] || continue
    if [ -n "$REAL" ]; then
      run setfacl -x "u:$ADMIN" "$f" 2>/dev/null; run setfacl -x "u:$DESK" "$f" 2>/dev/null
      if [ -d "$f" ]; then run setfacl -m u:waycompose:rx "$f"; else run setfacl -m u:waycompose:rw "$f"; fi
    else say "  (would drop u:$ADMIN, u:$DESK and add u:waycompose on $f)"; fi
  done

  for n in "${INSTALLS[@]}"; do initial_snapshot "$n"; run rm -f "$TRUST/$n/rotation-pending"; done
  say "6. the system services start; $DESK's agent listens"
  sys restart wc-bell-hub.service wc-sessiond.service
  if [ -n "$REAL" ]; then run loginctl enable-linger "$DESK"; fi
  as_user "$DESK" start wc-session-agent.socket
fi

# ----------------------------------------------------------- install-staged
# `wc-launch.sh deploy` with WC_LAUNCH_HOST_LAYOUT=system builds an
# installation in the admin's STAGE (BIN-DIR here) and calls this, as root:
#   wc-users-migrate.sh install-staged ADMIN auto STAGE NAME
# It replaces $VL/installs/NAME by the stage, as `apply` lays one out, and keeps
# only a root-generated broker-confirmed key. No data moves and no service restarts: a
# deployed installation's data is made by its applications, in the mapped
# user's home, at their first run.
if [ "$VERB" = install-staged ]; then
  n=${INSTALLS[0]}; st=$BINS
  nb="$VL/installs/$n"; nb_real=${nb#"$ROOT"}; st_real=${st#"$ROOT"}
  [ -f "$st/wc-session" ] && [ -f "$st/apps.ini" ] || { echo "$st is not a staged installation (no wc-session, apps.ini)"; exit 1; }
  case "$st_real" in "$AH_REAL"/*) ;; *) echo "the stage $st_real is not under ~$ADMIN"; exit 1 ;; esac
  if [ -n "$REAL" ]; then getent passwd waycompose >/dev/null || { echo "no account waycompose: this host has no system services"; exit 1; }; fi
  say "installation $n: $st_real -> $nb_real, its sessions as $DESK"
  tmp="$nb.new"
  run rm -rf "$tmp"
  run install -d -m 0755 "$VL/installs"
  run cp -a "$st" "$tmp"
  run rm -f "$tmp/session.key"
  run cp -f "$TRUST/$n/session.key" "$tmp/session.key"
  own root:root "$tmp"
  run chmod -R go-w "$tmp"
  if [ -n "$REAL" ]; then run chown waycompose:waycompose "$tmp/session.key"; else say "  (would chown waycompose $tmp/session.key)"; fi
  run chmod 0600 "$tmp/session.key"
  run sed -i -e "s|$st_real/|$nb_real/|g" -e "s|$st_real\$|$nb_real|g" -e "s|$AH_REAL/|~/|g" "$tmp/apps.ini"
  for f in gpu.env copy-scope dmabuf-node; do
    [ -f "$tmp/$f" ] && run sed -i "s|$st_real/|$nb_real/|g" "$tmp/$f"
    [ -f "$tmp/$f" ] && [ -z "$DRY" ] && grep -q "$AH_REAL/" "$tmp/$f" && { echo "  $tmp/$f still names $AH_REAL"; FAIL=1; }
  done
  if [ "$FAIL" = 0 ]; then
    if [ -e "$nb" ]; then
      if cmp -s "$nb/session.key" "$TRUST/$n/session.key"; then
        run install -d -m 0700 "$VL/protected-rollback"
        run rm -rf "$VL/protected-rollback/$n"
        run cp -a "$nb" "$VL/protected-rollback/$n"
      else
        secure_path "$TRUST/$n/rotation-pending" 600 && [ ! -e "$nb/session.key" ] || { echo "refusing snapshot of an untrusted verifier"; exit 1; }
      fi
      run mv "$nb" "$nb.old"
    fi
    run mv "$tmp" "$nb"
    run rm -rf "$nb.old"
    # GC roots made for the stage's store links: registered at their new path.
    for l in "$nb"/bin/*; do
      [ -L "$l" ] || continue
      t=$(readlink "$l"); case "$t" in /nix/store/*) ;; *) continue ;; esac
      if [ -n "$REAL" ]; then run nix-store --add-root "$l" --indirect -r "$t" >/dev/null; else say "  (would register GC root $l -> $t)"; fi
    done
    # The admin's copy of the key goes: the service's is the only one.
    run rm -f "$st/session.key"
    initial_snapshot "$n"
    run rm -f "$TRUST/$n/rotation-pending"
    if [ -n "$REAL" ]; then
      runuser -u "$DESK" -- env HOME="$DH" bash "$nb/wc-session" apps >/dev/null 2>&1 \
        && say "  every application checks out as $DESK" || { echo "  FAIL: wc-session apps as $DESK"; FAIL=1; }
    fi
  fi
fi

# ------------------------------------------------------------------- verify
if [ "$VERB" = verify ] || { [ "$VERB" = apply ] && [ "$FAIL" = 0 ]; }; then
  say "checking"
  chk() { if eval "$2"; then echo "  ok: $1"; else echo "  FAIL: $1"; FAIL=1; fi; }
  if [ -n "$REAL" ]; then
    chk "wc-bell-hub runs as waycompose" '[ "$(ps -o user= -p "$(systemctl show -p MainPID --value wc-bell-hub)")" = waycompose ]'
    chk "wc-sessiond runs as waycompose" '[ "$(ps -o user= -p "$(systemctl show -p MainPID --value wc-sessiond)")" = waycompose ]'
    chk "the broker listens" '[ -S /run/waycompose/broker.sock ]'
    chk "$DESK's agent listens" '[ -S /run/waycompose-agent/'"$DESK"'/agent.sock ]'
    chk "the admin runs no hub or service" '! systemctl --machine='"$ADMIN"'@.host --user is-active -q wc-sessiond.service wc-bell-hub.service'
    chk "users.conf was read" 'journalctl -u wc-sessiond -n 50 --no-pager | grep -q "sessions of"'
  fi
  for n in "${INSTALLS[@]}"; do
    nb="$VL/installs/$n"
    chk "$n: protected provenance and current verifier" 'trusted_key "$n" && cmp -s "$nb/session.key" "$TRUST/$n/session.key"'
    chk "$n: $nb" '[ -f "$nb/wc-session" ] && [ -f "$nb/apps.ini" ]'
    chk "$n: apps.ini names nothing of $ADMIN's" '! grep -q "$AH_REAL/" "$nb/apps.ini"'
    if [ -n "$REAL" ]; then
      chk "$n: the key is waycompose's, 0600" '[ "$(stat -c "%U %a" "$nb/session.key")" = "waycompose 600" ]'
      chk "$n: $DESK cannot read the key" '! runuser -u "$DESK" -- cat "$nb/session.key" >/dev/null 2>&1'
      # Every application's exec exists and is executable for DESKTOP, with ~ as theirs.
      chk "$n: every application checks out as $DESK" 'runuser -u "$DESK" -- env HOME="$DH" bash "$nb/wc-session" apps >/dev/null'
    fi
  done
fi

# --------------------------------------------------------------- remove-old
if [ "$VERB" = remove-old ]; then
  for n in "${INSTALLS[@]}"; do
    [ -L "$BACKUP/links/$n" ] || { echo "no backup link for $n: was it migrated?"; FAIL=1; continue; }
    base=$(readlink -f "$BACKUP/links/$n")
    [ -d "$VL/installs/$n" ] || { echo "$VL/installs/$n is missing; refusing"; FAIL=1; continue; }
    for u in $(user_data "$base/apps.ini" "${base#"$ROOT"}"); do
      [ -e "$DH/$u" ] && [ -e "$AH/$u" ] && run rm -rf "$AH/$u"
    done
    run rm -rf "$base"
    run rm -f "$BACKUP/links/$n" "$AH/.local/state/waycompose/sessiond/$n.seq"
  done
  [ -d "$AH/wc-bell" ] && run rm -rf "$AH/wc-bell"
  say "the admin's copies are gone; $BACKUP/units keeps the old unit files"
fi

# ----------------------------------------------------------------- rollback
if [ "$VERB" = rollback ]; then
  require_idle
  say "restore protected installation snapshots; keep system identity, broker and current keys"
  for n in "${INSTALLS[@]}"; do
    snap="$VL/protected-rollback/$n"; nb="$VL/installs/$n"
    secure_path "$VL/protected-rollback" 700 && [ -d "$snap" ] && [ ! -L "$snap" ] || { echo "no protected snapshot for $n"; exit 1; }
    [ -f "$snap/wc-session" ] && [ -f "$snap/apps.ini" ] || exit 1
    [ ! -e "$nb.rollback-new" ] || { echo "rollback staging already exists"; exit 1; }
  done
  sys stop wc-sessiond.service wc-bell-hub.service
  for n in "${INSTALLS[@]}"; do
    snap="$VL/protected-rollback/$n"; nb="$VL/installs/$n"
    run cp -a "$snap" "$nb.rollback-new"
    run rm -f "$nb.rollback-new/session.key"
    run cp -f "$TRUST/$n/session.key" "$nb.rollback-new/session.key"
    if [ -n "$REAL" ]; then run chown waycompose:waycompose "$nb.rollback-new/session.key"; fi
    run chmod 0600 "$nb.rollback-new/session.key"
    run mv "$nb" "$nb.rollback-current"
    run mv "$nb.rollback-new" "$nb"
    run rm -rf "$nb.rollback-current"
  done
  sys restart wc-bell-hub.service wc-sessiond.service
  say "protected rollback complete; admin units and raw-resource grants remain disabled"
fi

[ "$FAIL" = 0 ] && { say "done"; exit 0; }
say "FAILED (see above)"; exit 1
