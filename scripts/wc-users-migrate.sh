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
#   wc-users-migrate.sh rollback ADMIN DESKTOP BIN-DIR INSTALL...   back to the admin's user units
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
set -uo pipefail

VERB=${1:-}; ADMIN=${2:-}; DESK=${3:-}; BINS=${4:-}
shift 4 2>/dev/null || { sed -n '3,40p' "$0"; exit 2; }
INSTALLS=("$@")
[ ${#INSTALLS[@]} -gt 0 ] || { echo "no installation named"; exit 2; }
case "$VERB" in plan|apply|verify|remove-old|rollback) ;; *) echo "unknown verb $VERB"; exit 2 ;; esac

ROOT=${ROOT:-}
REAL=1; [ "$(id -u)" = 0 ] && [ -z "$ROOT" ] || REAL=""
homedir() { if [ -n "$ROOT" ]; then echo "$ROOT/home/$1"; else getent passwd "$1" | cut -d: -f6; fi; }
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
sys() { if [ -n "$REAL" ]; then run systemctl "$@"; else say "  (would: systemctl $*)"; fi; }

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

# ------------------------------------------------------------------- checks
if [ "$VERB" != rollback ] && [ "$VERB" != verify ] && [ "$VERB" != remove-old ]; then
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
    [ -e "$nb" ] && { echo "  $nb exists already; refusing to overwrite (rollback first)"; FAIL=1; continue; }
    data=$(user_data "$base/apps.ini" "$base_real")
    if echo "$data" | grep -q '^UNSUPPORTED:'; then
      echo "  apps.ini names $(echo "$data" | grep '^UNSUPPORTED:' | cut -d: -f2-): move it by hand"; FAIL=1; continue
    fi
    run cp -a "$base" "$nb"
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

  say "6. the system services start; $DESK's agent listens"
  sys restart wc-bell-hub.service wc-sessiond.service
  if [ -n "$REAL" ]; then run loginctl enable-linger "$DESK"; fi
  as_user "$DESK" start wc-session-agent.socket
fi

# ------------------------------------------------------------------- verify
if [ "$VERB" = verify ] || { [ "$VERB" = apply ] && [ "$FAIL" = 0 ]; }; then
  say "checking"
  chk() { if eval "$2"; then echo "  ok: $1"; else echo "  FAIL: $1"; FAIL=1; fi; }
  if [ -n "$REAL" ]; then
    chk "wc-bell-hub runs as waycompose" '[ "$(ps -o user= -p "$(systemctl show -p MainPID --value wc-bell-hub)")" = waycompose ]'
    chk "wc-sessiond runs as waycompose" '[ "$(ps -o user= -p "$(systemctl show -p MainPID --value wc-sessiond)")" = waycompose ]'
    chk "the broker listens" '[ -S /run/waycompose/broker.sock ]'
    chk "$DESK's agent listens" '[ -S /run/waycompose/agent/'"$DESK"'/agent.sock ]'
    chk "the admin runs no hub or service" '! systemctl --machine='"$ADMIN"'@.host --user is-active -q wc-sessiond.service wc-bell-hub.service'
    chk "users.conf was read" 'journalctl -u wc-sessiond -n 50 --no-pager | grep -q "sessions of"'
  fi
  for n in "${INSTALLS[@]}"; do
    nb="$VL/installs/$n"
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
  say "back to the admin's user units (also roll back the NixOS generation for the ACLs and the group)"
  sys stop wc-sessiond.service wc-bell-hub.service
  sys mask --runtime wc-sessiond.service wc-bell-hub.service
  for n in "${INSTALLS[@]}"; do
    [ -L "$BACKUP/links/$n" ] && run mv "$BACKUP/links/$n" "$AH/.config/waycompose/installs/$n"
    [ -d "$VL/installs/$n" ] && run mv "$VL/installs/$n" "$BACKUP/new-$n"
  done
  for u in "$BACKUP"/units/*.service; do [ -e "$u" ] && run mv "$u" "$AH/.config/systemd/user/"; done
  for f in "$ARENA_DIR" "$ARENA_DIR"/arena "$ARENA_DIR"/gpu "$ARENA_DIR"/*.sock; do
    [ -e "$f" ] || continue
    if [ -n "$REAL" ]; then
      if [ -d "$f" ]; then run setfacl -m "u:$ADMIN:rx" "$f"; else run setfacl -m "u:$ADMIN:rw" "$f"; fi
    fi
  done
  as_user "$ADMIN" daemon-reload
  as_user "$ADMIN" enable --now wc-bell-hub.service wc-sessiond.service
  say "the copies in ~$DESK stay (delete them by hand if wanted); the admin's were never removed before remove-old"
fi

[ "$FAIL" = 0 ] && { say "done"; exit 0; }
say "FAILED (see above)"; exit 1
