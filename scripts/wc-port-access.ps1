<#
  SUPERSEDED by the broker service (sv-guest broker, waycompose-design.md
  §111.8): it sets these ports, and the ivshmem devices, to SYSTEM and
  Administrators only and opens them for the user's processes. This script is
  what WC_LAUNCH_BROKER=skip still runs.

  wc-port-access.ps1 -- let ONE user's medium-integrity processes open the two
  virtio-serial ports waycompose rings: org.sirati.wcbell.0 (the Wayland rings'
  doorbell, waycompose-design.md §89) and org.sirati.svbell.0 (the GPU sessions'
  doorbell, §87). docs/security-model.md R32 is the argument; §91 the record.

  vioser creates each port as a raw PDO with the default descriptor
      D:P(A;;FA;;;SY)(A;;FA;;;BA)S:AI(ML;;NW;;;LW)
  (SYSTEM and Administrators only), so a shortcut started by Explorer, which
  runs at medium integrity with a filtered token, gets ERROR_ACCESS_DENIED and
  the session falls back to ssh and to no GPU doorbell (§90.3). This replaces
  the DACL of exactly those two device objects with
      D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFW;;;<user SID>)S:(ML;;NWNR;;;ME)
  i.e. the original two entries, plus read and write (no WRITE_DAC, no DELETE,
  no WRITE_OWNER) for one named account, and a Medium label that refuses read
  and write to Low-integrity processes. AppContainers match no entry.

  It is set twice, because each half covers what the other cannot:
    * the device's PnP "Security" property (SPDRP_SECURITY_SDS), which the PnP
      manager applies to the device object every time the device starts: every
      boot and every vioser driver update, with no component of ours running;
    * the live device object (SetSecurityInfo on a handle), so the change is
      effective now, without restarting a device somebody may be using.
  `install` also registers a SYSTEM task that runs `apply` at startup, which
  re-finds the ports by NAME, so a port that QEMU renumbers still gets it. The
  task's copy of this script lives under Program Files, which a medium process
  cannot write: a SYSTEM task that ran a user-writable script would hand that
  user SYSTEM.

    -Action show                       the live and the PnP descriptor of each port
    -Action apply   -User <name|SID>   set both (needs Administrators or SYSTEM)
    -Action revert                     restore the original descriptor, drop the property
    -Action install -User <name|SID>   apply, copy this script to Program Files, register the task
    -Action uninstall                  revert, remove the task and the copy

  A HELD PORT CANNOT BE OPENED, EVEN FOR READ_CONTROL, AND THAT IS WHAT MAKES
  THIS SAFE TO RUN BESIDE A SESSION. vioser's own create handler would answer a
  second open with STATUS_OBJECT_NAME_EXISTS (a success code) and its close
  handler would then clear GuestConnected for the real holder -- but vioser also
  makes each port exclusive (WdfDeviceInitSetExclusive), so the I/O manager
  refuses the second open with ERROR_ACCESS_DENIED before vioser sees it.
  Measured (§91.2): a holder that asked vioser for its state before and after a
  READ_CONTROL open of its port read guest_connected=1 both times, and the open
  failed with 5. So a held port's live descriptor is simply left alone and
  reported; the PnP property is written regardless and takes effect at the
  device's next start, or when this is run again with the port free.
#>
param(
  [ValidateSet("show", "apply", "revert", "install", "uninstall")][string]$Action = "show",
  [string]$User = "",
  [string[]]$Ports = @("org.sirati.wcbell.0", "org.sirati.svbell.0")
)
# Through `powershell -File` (the boot task) `-Ports a,b` arrives as ONE
# string; split it here, or the task looks for a port named "a,b" and does
# nothing (§99.6: it had done nothing at every startup since §91).
$Ports = @($Ports | ForEach-Object { $_ -split "," } | Where-Object { $_ })
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

# What vioser gives a port, recorded on testhost's guest on 2026-10-16
# (vioser from virtio-win, ports 4 and 5 of the VM's virtio-serial device).
$Original = "D:P(A;;FA;;;SY)(A;;FA;;;BA)S:AI(ML;;NW;;;LW)"
$TaskName = "waycompose port access"
$InstallDir = Join-Path $env:ProgramFiles "waycompose"

Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices; using System.Text;
public static class WcPorts {
  [StructLayout(LayoutKind.Sequential)] public struct DEVINFO { public uint cb; public Guid cls; public uint inst; public IntPtr res; }
  [DllImport("kernel32", CharSet=CharSet.Unicode, SetLastError=true)] static extern uint QueryDosDeviceW(string n, StringBuilder b, uint m);
  [DllImport("kernel32", CharSet=CharSet.Unicode, SetLastError=true)] static extern IntPtr CreateFileW(string n, uint a, uint s, IntPtr sa, uint d, uint f, IntPtr t);
  [DllImport("kernel32")] static extern bool CloseHandle(IntPtr h);
  [DllImport("kernel32")] static extern IntPtr LocalFree(IntPtr p);
  [DllImport("advapi32", SetLastError=true)] static extern uint GetSecurityInfo(IntPtr h, int t, uint si, IntPtr o, IntPtr g, IntPtr d, IntPtr s, out IntPtr sd);
  [DllImport("advapi32", SetLastError=true)] static extern uint SetSecurityInfo(IntPtr h, int t, uint si, IntPtr o, IntPtr g, IntPtr d, IntPtr s);
  [DllImport("advapi32", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool ConvertSecurityDescriptorToStringSecurityDescriptorW(IntPtr sd, uint rev, uint si, out IntPtr s, IntPtr l);
  [DllImport("advapi32", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool ConvertStringSecurityDescriptorToSecurityDescriptorW(string s, uint rev, out IntPtr sd, IntPtr l);
  [DllImport("advapi32", SetLastError=true)] static extern bool GetSecurityDescriptorDacl(IntPtr sd, out bool present, out IntPtr dacl, out bool def);
  [DllImport("advapi32", SetLastError=true)] static extern bool GetSecurityDescriptorSacl(IntPtr sd, out bool present, out IntPtr sacl, out bool def);
  [DllImport("setupapi", SetLastError=true)] static extern IntPtr SetupDiCreateDeviceInfoList(IntPtr g, IntPtr w);
  [DllImport("setupapi", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool SetupDiOpenDeviceInfoW(IntPtr set, string id, IntPtr w, uint f, ref DEVINFO d);
  [DllImport("setupapi", SetLastError=true)] static extern bool SetupDiGetDeviceRegistryPropertyW(IntPtr set, ref DEVINFO d, uint p, out uint t, byte[] b, uint n, out uint need);
  [DllImport("setupapi", SetLastError=true)] static extern bool SetupDiSetDeviceRegistryPropertyW(IntPtr set, ref DEVINFO d, uint p, byte[] b, uint n);
  [DllImport("setupapi")] static extern bool SetupDiDestroyDeviceInfoList(IntPtr set);
  const uint SPDRP_SECURITY = 0x17, SPDRP_SECURITY_SDS = 0x18;
  const uint READ_CONTROL = 0x20000, WRITE_DAC = 0x40000, WRITE_OWNER = 0x80000;
  const uint DACL_SI = 0x4, LABEL_SI = 0x10, PROTECTED_DACL_SI = 0x80000000;
  const int SE_KERNEL_OBJECT = 6;

  // \\.\Global\<name> is a symbolic link to the port's PDO, \Device\000000NN.
  public static string Target(string name) {
    var b = new StringBuilder(1024);
    if (QueryDosDeviceW("Global\\" + name, b, 1024) == 0) return null;
    return b.ToString();
  }
  static string ToSddl(IntPtr sd, uint si) {
    IntPtr s; if (!ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, 1, si, out s, IntPtr.Zero)) return "unconvertible";
    string r = Marshal.PtrToStringUni(s); LocalFree(s); return r;
  }
  // The live descriptor of the device object behind \\.\Global\<name>.
  public static string Live(string name) {
    IntPtr h = CreateFileW("\\\\.\\Global\\" + name, READ_CONTROL, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
    if (h == (IntPtr)(-1)) return "unreadable (error " + Marshal.GetLastWin32Error() + Held() + ")";
    IntPtr sd; uint r = GetSecurityInfo(h, SE_KERNEL_OBJECT, DACL_SI | LABEL_SI, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero, out sd);
    CloseHandle(h);
    if (r != 0) return "unreadable (GetSecurityInfo error " + r + ")";
    string s = ToSddl(sd, DACL_SI | LABEL_SI); LocalFree(sd); return s;
  }
  static string Held() { return "; vioser ports are exclusive, so 5 here, as an administrator, means another process holds it open"; }
  // Replace the live DACL and label. Returns null on success, or why not.
  public static string SetLive(string name, string sddl) {
    IntPtr sd; if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl, 1, out sd, IntPtr.Zero)) return "bad SDDL (error " + Marshal.GetLastWin32Error() + ")";
    try {
      bool p, d; IntPtr dacl, sacl;
      GetSecurityDescriptorDacl(sd, out p, out dacl, out d);
      GetSecurityDescriptorSacl(sd, out p, out sacl, out d);
      IntPtr h = CreateFileW("\\\\.\\Global\\" + name, READ_CONTROL | WRITE_DAC | WRITE_OWNER, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
      if (h == (IntPtr)(-1)) return "not opened (error " + Marshal.GetLastWin32Error() + Held() + ")";
      uint si = DACL_SI | PROTECTED_DACL_SI | (sacl != IntPtr.Zero ? LABEL_SI : 0);
      uint r = SetSecurityInfo(h, SE_KERNEL_OBJECT, si, IntPtr.Zero, IntPtr.Zero, dacl, sacl);
      CloseHandle(h);
      return r == 0 ? null : "SetSecurityInfo error " + r;
    } finally { LocalFree(sd); }
  }
  static IntPtr Open(string inst, ref DEVINFO d) {
    IntPtr set = SetupDiCreateDeviceInfoList(IntPtr.Zero, IntPtr.Zero);
    d.cb = (uint)Marshal.SizeOf(typeof(DEVINFO));
    if (!SetupDiOpenDeviceInfoW(set, inst, IntPtr.Zero, 0, ref d)) { int e = Marshal.GetLastWin32Error(); SetupDiDestroyDeviceInfoList(set); throw new Exception("SetupDiOpenDeviceInfo(" + inst + ") error " + e); }
    return set;
  }
  // The PnP Security property, as SDDL; "" when the device has none.
  public static string Property(string inst) {
    var d = new DEVINFO(); IntPtr set = Open(inst, ref d);
    try {
      uint t, need; var buf = new byte[4096];
      if (!SetupDiGetDeviceRegistryPropertyW(set, ref d, SPDRP_SECURITY, out t, buf, (uint)buf.Length, out need)) {
        int e = Marshal.GetLastWin32Error(); return e == 13 ? "" : "unreadable (error " + e + ")"; // ERROR_INVALID_DATA: none set
      }
      IntPtr p = Marshal.AllocHGlobal((int)need); Marshal.Copy(buf, 0, p, (int)need);
      string s = ToSddl(p, DACL_SI | LABEL_SI); Marshal.FreeHGlobal(p); return s;
    } finally { SetupDiDestroyDeviceInfoList(set); }
  }
  // Write it (sddl) or remove it (null).
  public static void SetProperty(string inst, string sddl) {
    var d = new DEVINFO(); IntPtr set = Open(inst, ref d);
    try {
      bool ok;
      if (sddl == null) ok = SetupDiSetDeviceRegistryPropertyW(set, ref d, SPDRP_SECURITY, null, 0);
      else { var b = Encoding.Unicode.GetBytes(sddl + "\0"); ok = SetupDiSetDeviceRegistryPropertyW(set, ref d, SPDRP_SECURITY_SDS, b, (uint)b.Length); }
      if (!ok) { int e = Marshal.GetLastWin32Error(); if (!(sddl == null && e == 13)) throw new Exception("SetupDiSetDeviceRegistryProperty(" + inst + ") error " + e); }
    } finally { SetupDiDestroyDeviceInfoList(set); }
  }
}
"@

# Write-Host, not Write-Output: the lines must not become a function's return
# value (Apply-All returns a count).
function Say($s) {
  $line = "[wc-port-access] " + $s
  Write-Host $line
  # §99.6: the boot task's lines go to a log beside the installed script,
  # because a SYSTEM task at startup has nobody to print to.
  if ($Action -eq "apply" -and (Test-Path $InstallDir)) {
    Add-Content -Path (Join-Path $InstallDir "wc-port-access.log") -Value ((Get-Date -Format "yyyy-MM-dd HH:mm:ss") + " " + $line) -ErrorAction SilentlyContinue
  }
}

# Find a port's devnode by its name: the symbolic link's target is the PDO's
# name, and exactly one present device reports that PDO name.
function Find-Port([string]$Name) {
  $pdo = [WcPorts]::Target($Name)
  if (-not $pdo) { return $null }
  foreach ($d in Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -match '\\VIOSERIALPORT\\' }) {
    $p = (Get-PnpDeviceProperty -InstanceId $d.InstanceId -KeyName DEVPKEY_Device_PDOName).Data
    if ($p -eq $pdo) { return [pscustomobject]@{ Name = $Name; Pdo = $pdo; Instance = $d.InstanceId } }
  }
  return $null
}

# §99.6. A port's name exists only while it is started (it is the target of
# \.\Global\<name>), so a port that is disabled cannot be found by name. Every
# run records, for each of our ports it can see, which device it is; the boot
# task re-enables a recorded port that is disabled before it applies anything.
# A disable that waited for a reboot (`pnputil /disable-device` answering
# 3010, as on 2026-10-17) would otherwise come up as no doorbell at all.
$PortMap = Join-Path $InstallDir "ports.json"

function Record-Ports {
  $map = @{}
  if (Test-Path $PortMap) {
    (Get-Content -Raw $PortMap | ConvertFrom-Json).psobject.Properties | ForEach-Object { $map[$_.Name] = $_.Value }
  }
  $changed = $false
  foreach ($n in $Ports) {
    try { $p = Find-Port $n } catch { Say "${n}: could not be looked up ($_)"; continue }
    if ($p -and $map[$n] -ne $p.Instance) { $map[$n] = $p.Instance; $changed = $true }
  }
  if ($changed -and (Test-Path $InstallDir)) {
    ($map | ConvertTo-Json) | Set-Content -Path $PortMap -Encoding ASCII
    Say ("recorded our ports: " + (($map.GetEnumerator() | ForEach-Object { $_.Key + " = " + $_.Value }) -join "; "))
  }
}

function Enable-Ours {
  if (-not (Test-Path $PortMap)) { Say "no record of our ports yet ($PortMap); nothing to re-enable"; return }
  $map = Get-Content -Raw $PortMap | ConvertFrom-Json
  foreach ($n in $Ports) {
    $inst = $map.$n
    if (-not $inst) { Say "${n}: not recorded; not re-enabled"; continue }
    $flags = (Get-ItemProperty -Path ("HKLM:\SYSTEM\CurrentControlSet\Enum\" + $inst) -Name ConfigFlags -ErrorAction SilentlyContinue).ConfigFlags
    $d = Get-PnpDevice -InstanceId $inst -ErrorAction SilentlyContinue
    if (-not $d) { Say "${n}: recorded device $inst is not present; skipped"; continue }
    if ((($flags -band 1) -eq 0) -and $d.Problem -ne 22) { Say "${n}: enabled ($inst)"; continue }
    # Not under "Stop": Windows PowerShell 5 turns a native command's stderr
    # into a terminating error there, and this step died silently on the one
    # port it exists for.
    $eap = $ErrorActionPreference; $ErrorActionPreference = "Continue"
    try {
      $out = (& pnputil.exe /enable-device "$inst" 2>&1 | ForEach-Object { "$_" } | Out-String).Trim() -replace "\s+", " "
      $code = $LASTEXITCODE
    } catch { $out = "$_"; $code = -1 } finally { $ErrorActionPreference = $eap }
    Say ("${n}: was disabled (" + $inst + ", ConfigFlags 0x" + ("{0:x}" -f [int]$flags) + ", problem " + $d.Problem + "); pnputil /enable-device exit " + $code + ": " + $out)
  }
}

function Resolve-Sid([string]$U) {
  if (-not $U) { throw "-User is required for $Action (the account whose processes may open the ports)" }
  if ($U -match '^S-1-') { return (New-Object Security.Principal.SecurityIdentifier $U).Value }
  return (New-Object Security.Principal.NTAccount $U).Translate([Security.Principal.SecurityIdentifier]).Value
}

function Granted([string]$Sid) { "D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFW;;;$Sid)S:(ML;;NWNR;;;ME)" }

function Apply-All([string]$Sddl, [bool]$DropProperty) {
  $bad = 0
  foreach ($n in $Ports) {
    $p = Find-Port $n
    # Retried, because at startup the task can run before vioser has made the link.
    for ($i = 0; -not $p -and $i -lt 30 -and $Action -eq "apply"; $i++) { Start-Sleep -Seconds 2; $p = Find-Port $n }
    if (-not $p) { Say "${n}: no such port on this machine (no \\.\Global\$n); skipped"; continue }
    if ($DropProperty) { [WcPorts]::SetProperty($p.Instance, $null) } else { [WcPorts]::SetProperty($p.Instance, $Sddl) }
    $why = [WcPorts]::SetLive($n, $Sddl)
    $live = [WcPorts]::Live($n)
    if ($why) { Say "${n}: PnP property written; live descriptor NOT changed: $why"; $bad++ }
    Say ("{0} ({1}): live {2}; PnP property {3}" -f $n, $p.Instance, $live, ([WcPorts]::Property($p.Instance) | ForEach-Object { if ($_) { $_ } else { "none" } }))
  }
  return [int]$bad
}

# Recorded first, on every action, so that a port whose descriptor cannot be
# changed (held open) is still known by device for the next boot.
if (Test-Path $InstallDir) { Record-Ports }

switch ($Action) {
  "show" {
    foreach ($n in $Ports) {
      $p = Find-Port $n
      if (-not $p) { Say "${n}: no such port on this machine"; continue }
      $prop = [WcPorts]::Property($p.Instance); if (-not $prop) { $prop = "none (vioser's default applies at start)" }
      Say ("{0} -> {1} ({2})" -f $n, $p.Pdo, $p.Instance)
      Say ("    live:          " + [WcPorts]::Live($n))
      Say ("    PnP property:  " + $prop)
    }
    $t = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if ($t) { Say ("task '{0}': {1}, runs {2} {3}" -f $TaskName, $t.State, $t.Actions[0].Execute, $t.Actions[0].Arguments) } else { Say "task '$TaskName': not registered" }
  }
  "apply" {
    $sid = Resolve-Sid $User
    try { Enable-Ours } catch { Say "re-enabling our ports failed: $_" }
    Say "granting read and write on $($Ports -join ', ') to $sid (medium integrity and up)"
    exit (Apply-All (Granted $sid) $false)
  }
  "revert" {
    Say "restoring $Original on $($Ports -join ', ') and removing the PnP property"
    exit (Apply-All $Original $true)
  }
  "install" {
    $sid = Resolve-Sid $User
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $bad = Apply-All (Granted $sid) $false
    # Administrators and SYSTEM may write it, everybody else may only read it:
    # the task below runs this file as SYSTEM.
    icacls $InstallDir /inheritance:r /grant:r "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-544:(OI)(CI)F" "*S-1-5-32-545:(OI)(CI)RX" | Out-Null
    $dst = Join-Path $InstallDir "wc-port-access.ps1"
    if ($PSCommandPath -ne $dst) { Copy-Item -Force $PSCommandPath $dst }
    $arg = "-NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$dst`" -Action apply -User $sid -Ports " + ($Ports -join ",")
    $a = New-ScheduledTaskAction -Execute "powershell.exe" -Argument $arg
    $tr = New-ScheduledTaskTrigger -AtStartup
    $pr = New-ScheduledTaskPrincipal -UserId "S-1-5-18" -LogonType ServiceAccount -RunLevel Highest
    $st = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 5)
    Register-ScheduledTask -TaskName $TaskName -Action $a -Trigger $tr -Principal $pr -Settings $st -Force | Out-Null
    Say "installed: $dst, and the SYSTEM task '$TaskName' that re-applies it at every startup"
    exit $bad
  }
  "uninstall" {
    $bad = Apply-All $Original $true
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
    Remove-Item -Force -Recurse -ErrorAction SilentlyContinue $InstallDir
    Say "uninstalled: the original descriptor is back, the task and $InstallDir are gone"
    exit $bad
  }
}
