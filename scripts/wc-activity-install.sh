#!/usr/bin/env bash
# wc-activity-install.sh: build wc-activity.exe, put it on the guest and start it in
# <guest-user>'s interactive session, now and at every logon (docs/waycompose-design.md §101).
# No window, no focus, no input: safe to run while somebody uses the desktop.
. "$(dirname "${BASH_SOURCE[0]}")/site.sh"
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
G="ssh -o BatchMode=yes -o ConnectTimeout=8 -o LogLevel=ERROR $SITE_GUEST"
OUT=${WC_ACTIVITY_OUT:-$(mktemp -d)}
x86_64-w64-mingw32-gcc -O2 -Wall -Wextra -Werror -mwindows -municode -o "$OUT/wc-activity.exe" "$ROOT/scripts/wc-activity.c"
DIR=$SITE_GUEST_MSYS_HOME/AppData/Local/waycompose/tools
$G "mkdir -p $DIR"
# A running copy holds its file open; stop it before replacing it.
$G 'powershell -NoProfile -Command "Get-Process wc-activity -ErrorAction SilentlyContinue | Stop-Process -Force; Start-Sleep -Milliseconds 500"' >/dev/null 2>&1 || true
$G "cat > $DIR/wc-activity.exe" < "$OUT/wc-activity.exe"
ps='$exe = "$env:LOCALAPPDATA\waycompose\tools\wc-activity.exe"
$a = New-ScheduledTaskAction -Execute $exe
$t = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$p = New-ScheduledTaskPrincipal -UserId ([Security.Principal.WindowsIdentity]::GetCurrent().Name) -LogonType Interactive -RunLevel Limited
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 99 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName "wc-activity" -Action $a -Trigger $t -Principal $p -Settings $s -Force | Out-Null
Start-ScheduledTask -TaskName "wc-activity"
Start-Sleep -Seconds 4
Get-Process wc-activity -ErrorAction SilentlyContinue | ForEach-Object { "running pid " + $_.Id + " session " + $_.SessionId }
Get-Content "$env:LOCALAPPDATA\waycompose\activity.txt" -TotalCount 7 -ErrorAction SilentlyContinue'
enc=$(printf '%s' "$ps" | iconv -t UTF-16LE | base64 -w0)
$G "powershell -NoProfile -EncodedCommand $enc" 2>&1 | tr -d '\r' | grep -v "CLIXML\|<Objs"
