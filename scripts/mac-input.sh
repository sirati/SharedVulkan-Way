#!/usr/bin/env bash
# Input into the macOS VM through QEMU (QMP input-send-event), i.e. through the
# virtual USB keyboard and tablet, exactly as a person's input arrives; no
# guest permission (Accessibility, TCC) is involved.
#
#   mac-input.sh click X Y [left|right|middle]   (screen pixels of the 1280x800 display)
#   mac-input.sh move X Y
#   mac-input.sh drag X1 Y1 X2 Y2
#   mac-input.sh type 'text'                       (US layout; letters, digits, space, - . / _ =)
#   mac-input.sh key [MOD+]...KEY                  (qcodes: ret, esc, tab, a, meta_l, ctrl, shift ...)
set -euo pipefail
inst=${MACVM_INSTANCE:-wayland-host}
q=/mnt/datasets/macvm/instances/$inst/qmp.sock
W=${MAC_SCREEN_W:-1280}; H=${MAC_SCREEN_H:-800}
qmp() { { echo '{"execute":"qmp_capabilities"}'; for c in "$@"; do echo "$c"; done; } | socat -t 2 - "UNIX-CONNECT:$q" >/dev/null; }
abs() { echo $(( $1 * 32767 / ($2 - 1) )); }
pos() { echo "{\"type\":\"abs\",\"data\":{\"axis\":\"x\",\"value\":$(abs "$1" "$W")}},{\"type\":\"abs\",\"data\":{\"axis\":\"y\",\"value\":$(abs "$2" "$H")}}"; }
ev() { echo "{\"execute\":\"input-send-event\",\"arguments\":{\"events\":[$1]}}"; }
btn() { echo "{\"type\":\"btn\",\"data\":{\"down\":$2,\"button\":\"$1\"}}"; }
keyev() { echo "{\"type\":\"key\",\"data\":{\"down\":$2,\"key\":{\"type\":\"qcode\",\"data\":\"$1\"}}}"; }
qcode() {
  case "$1" in
    ' ') echo spc;; '-') echo minus;; '.') echo dot;; '/') echo slash;; '=') echo equal;; ',') echo comma;;
    [a-z0-9]) echo "$1";; *) echo "";;
  esac
}
case "${1:-}" in
  move) qmp "$(ev "$(pos "$2" "$3")")" ;;
  click)
    b=${4:-left}
    qmp "$(ev "$(pos "$2" "$3")")"; sleep 0.05
    qmp "$(ev "$(btn "$b" true)")"; sleep 0.08; qmp "$(ev "$(btn "$b" false)")" ;;
  drag)
    qmp "$(ev "$(pos "$2" "$3")")"; sleep 0.05; qmp "$(ev "$(btn left true)")"
    for i in 1 2 3 4 5 6 7 8 9 10; do
      x=$(( $2 + ($4 - $2) * i / 10 )); y=$(( $3 + ($5 - $3) * i / 10 ))
      qmp "$(ev "$(pos "$x" "$y")")"; sleep 0.03
    done
    qmp "$(ev "$(btn left false)")" ;;
  type)
    s=$2
    for ((i = 0; i < ${#s}; i++)); do
      ch=${s:i:1}
      if [ "$ch" = "_" ]; then
        qmp "$(ev "$(keyev shift true),$(keyev minus true)")" "$(ev "$(keyev minus false),$(keyev shift false)")"
      elif [[ "$ch" =~ [A-Z] ]]; then
        lc=$(echo "$ch" | tr A-Z a-z)
        qmp "$(ev "$(keyev shift true),$(keyev "$lc" true)")" "$(ev "$(keyev "$lc" false),$(keyev shift false)")"
      else
        k=$(qcode "$ch"); [ -n "$k" ] || { echo "mac-input: cannot type '$ch'" >&2; exit 2; }
        qmp "$(ev "$(keyev "$k" true)")" "$(ev "$(keyev "$k" false)")"
      fi
      sleep 0.02
    done ;;
  key)
    IFS=+ read -r -a parts <<<"$2"
    down=""; up=""
    for p in "${parts[@]}"; do down+="${down:+,}$(keyev "$p" true)"; done
    for ((i = ${#parts[@]} - 1; i >= 0; i--)); do up+="${up:+,}$(keyev "${parts[i]}" false)"; done
    qmp "$(ev "$down")"; sleep 0.08; qmp "$(ev "$up")" ;;
  *) sed -n '2,12p' "$0"; exit 2 ;;
esac
