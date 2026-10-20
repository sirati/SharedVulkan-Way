<#
  wc-broker-install.ps1 -- install, show or remove the waycompose broker, the
  one SYSTEM service of a Windows waycompose guest (waycompose-design.md
  §111.8, security-model.md R44). Run as an administrator; scripts/wc-launch.sh
  deploy runs it over its administrator channel.

    -Action install -Source <dir>   copy sv-guest.exe from <dir> into Program
                                    Files, (re)create and start the service,
                                    remove wc-port-access's task (§91)
    -Action key -Install <name>     read 64 hex digits on stdin and store them
                                    as the installation's key for the broker
    -Action show                    the service, its log's tail, the devices
    -Action uninstall               stop and delete the service, give the
                                    devices their descriptors from before,
                                    let the holder go

  WHY PROGRAM FILES. The service runs as SYSTEM. If its binary were in a
  directory its user can write (the user's install directory), that user
  could replace it and be SYSTEM at the next start. Program Files\waycompose
  admits SYSTEM and Administrators to write, everyone else to read and run.

  A RUNNING BINARY CANNOT BE OVERWRITTEN, BUT IT CAN BE RENAMED. The holder
  (`sv-guest broker holder`) keeps running across a service restart, by
  design, so it may hold the old sv-guest.exe open; the old file is renamed
  aside and deleted when nothing holds it any more.
#>
param(
  [ValidateSet("install", "key", "show", "uninstall")][string]$Action = "show",
  [string]$Source = "",
  [string]$Install = ""
)
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Name = "waycompose-broker"
$Dir = Join-Path $env:ProgramFiles "waycompose"
$Exe = Join-Path $Dir "sv-guest.exe"
$Data = Join-Path $env:ProgramData "waycompose"
$Keys = Join-Path $Data "keys"
$OldTask = "waycompose port access"
$Before = Join-Path $Data "pre-broker"
$LegacyFiles = @("wc-port-access.ps1", "wc-port-access.log", "ports.json")

function Say($s) { Write-Host "[wc-broker-install] $s" }

# Native commands' stderr must not stop the script (Windows PowerShell 5).
function Run([string]$exe, [string[]]$a) {
  $eap = $ErrorActionPreference; $ErrorActionPreference = "Continue"
  try { $out = (& $exe @a 2>&1 | ForEach-Object { "$_" } | Out-String).Trim() -replace "\s+", " "; $code = $LASTEXITCODE }
  finally { $ErrorActionPreference = $eap }
  return @($code, $out)
}

# Ownership grants WRITE_DAC independently of the DACL. Replace both the
# owner and the entire DACL, including explicit grants from an older install.
function Secure-Path([string]$p, [bool]$Directory, [bool]$UsersRead) {
  $acl = Get-Acl -LiteralPath $p
  $inherit = if ($Directory) { "OICI" } else { "" }
  $sddl = "O:BAD:P(A;$inherit;FA;;;SY)(A;$inherit;FA;;;BA)"
  if ($UsersRead) { $sddl += "(A;$inherit;FRFX;;;BU)" }
  $sections = [Security.AccessControl.AccessControlSections]::Owner -bor [Security.AccessControl.AccessControlSections]::Access
  $acl.SetSecurityDescriptorSddlForm($sddl, $sections)
  Set-Acl -LiteralPath $p -AclObject $acl
}

function Secure-Dir([string]$d, [bool]$UsersRead) {
  New-Item -ItemType Directory -Force -Path $d | Out-Null
  Secure-Path $d $true $UsersRead
}

switch ($Action) {
  "install" {
    if (-not $Source) { throw "-Source is required" }
    $src = Join-Path $Source "sv-guest.exe"
    if (-not (Test-Path $src)) { throw "no $src" }
    Secure-Dir $Dir $true
    Secure-Dir $Data $false
    Secure-Dir $Keys $false
    $svc = Get-Service -Name $Name -ErrorAction SilentlyContinue
    if ($svc -and $svc.Status -ne "Stopped") {
      Stop-Service -Name $Name -Force -ErrorAction SilentlyContinue
      (Get-Service -Name $Name).WaitForStatus("Stopped", (New-TimeSpan -Seconds 15))
    }
    Get-ChildItem -Path $Dir -Filter "sv-guest.exe.old-*" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
    if (Test-Path $Exe) {
      try { Remove-Item -Force $Exe } catch { Rename-Item -Force $Exe ("sv-guest.exe.old-" + (Get-Date -Format "yyyyMMddHHmmss")) }
    }
    Copy-Item -Force $src $Exe
    Secure-Path $Exe $false $true
    # Not `sc.exe create binPath= "..."`: Windows PowerShell 5 passes the
    # quotes a path with a space needs to a native program mangled.
    $bin = "`"$Exe`" broker"
    if ($svc) {
      Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Services\$Name" -Name ImagePath -Value $bin
      Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Services\$Name" -Name ObjectName -Value "LocalSystem"
      Set-Service -Name $Name -StartupType Automatic
    } else {
      New-Service -Name $Name -BinaryPathName $bin -DisplayName "waycompose broker" -StartupType Automatic | Out-Null
    }
    Run sc.exe @("description", $Name, "Lends the waycompose devices and memory to each user's own processes (waycompose-design.md 111.8)") | Out-Null
    # Restarted on failure: 1 s, 5 s, then every 30 s; the count resets daily.
    $r = Run sc.exe @("failure", $Name, "reset=", "86400", "actions=", "restart/1000/restart/5000/restart/30000")
    if ($r[0] -ne 0) { throw "sc.exe failure: $($r[1])" }
    Run sc.exe @("failureflag", $Name, "1") | Out-Null
    # Keep the first baseline across upgrades and repeated installations.
    if (-not (Test-Path (Join-Path $Before "complete"))) {
      if (Test-Path $Before) { throw "incomplete pre-broker backup at $Before; restore or inspect it before retrying" }
      Secure-Dir $Before $false
      $task = Get-ScheduledTask -TaskName $OldTask -ErrorAction SilentlyContinue
      if ($task) {
        Export-ScheduledTask -TaskName $OldTask | Set-Content -Encoding Unicode (Join-Path $Before "task.xml")
      }
      foreach ($f in $LegacyFiles) {
        $p = Join-Path $Dir $f
        if (Test-Path $p) { Copy-Item -Force $p (Join-Path $Before $f) }
      }
      New-Item -ItemType File -Path (Join-Path $Before "complete") -Force | Out-Null
      Get-ChildItem -File $Before | ForEach-Object { Secure-Path $_.FullName $false $false }
    }
    # §91's per-user grant is superseded: the broker sets the devices'
    # descriptors itself at every start.
    if (Get-ScheduledTask -TaskName $OldTask -ErrorAction SilentlyContinue) {
      Unregister-ScheduledTask -TaskName $OldTask -Confirm:$false
      Say "removed the task '$OldTask' (wc-port-access.ps1, §91)"
    }
    foreach ($f in $LegacyFiles) {
      Remove-Item -Force -ErrorAction SilentlyContinue (Join-Path $Dir $f)
    }
    Start-Service -Name $Name
    (Get-Service -Name $Name).WaitForStatus("Running", (New-TimeSpan -Seconds 15))
    Start-Sleep -Milliseconds 500
    $log = Join-Path $Data "broker.log"
    if (Test-Path $log) { Get-Content -Tail 12 $log | ForEach-Object { Say "log: $_" } }
    Say "installed: $Exe as service $Name (LocalSystem, automatic, restarted on failure)"
  }
  "key" {
    if ($Install -notmatch '^[A-Za-z0-9_-]{1,32}$') { throw "-Install must be an installation name" }
    $k = (@($input) -join "").Trim()
    if (-not $k) { $k = [Console]::In.ReadToEnd().Trim() }
    if ($k -notmatch '^[0-9a-f]{64}$') { throw "the key on stdin is not 64 hex digits" }
    Secure-Dir $Data $false
    Secure-Dir $Keys $false
    $p = Join-Path $Keys "$Install.key"
    [IO.File]::WriteAllText($p, $k)
    Secure-Path $p $false $false
    Say "key of installation $Install stored in $p (SYSTEM and Administrators only)"
  }
  "show" {
    $svc = Get-Service -Name $Name -ErrorAction SilentlyContinue
    if ($svc) { Say "service: $($svc.Status), $($svc.StartType)" } else { Say "service: not installed" }
    $r = Run sc.exe @("qc", $Name); Say "sc qc: $($r[1])"
    if (Test-Path $Exe) { & $Exe broker show | ForEach-Object { Say "device: $_" } }
    $log = Join-Path $Data "broker.log"
    if (Test-Path $log) { Get-Content -Tail 20 $log | ForEach-Object { Say "log: $_" } }
    if (Test-Path $Keys) { Get-ChildItem $Keys | ForEach-Object { Say "key: $($_.Name)" } }
  }
  "uninstall" {
    if (-not (Test-Path $Exe)) { throw "broker executable missing; cannot prove safe uninstall" }
    $svc = Get-Service -Name $Name -ErrorAction SilentlyContinue
    $wasRunning = $svc -and ($svc.Status -eq 'Running')
    try {
      # The holder serves one broker connection. Stop the broker to query it,
      # keeping its service registration and every protection in place.
      if ($svc -and $svc.Status -ne 'Stopped') {
        Stop-Service -Name $Name -Force
        (Get-Service -Name $Name).WaitForStatus("Stopped", (New-TimeSpan -Seconds 15))
      }
      $quit = Run $Exe @("broker", "holder-quit")
      if ($quit[0] -ne 0) { throw "holder did not prove empty: $($quit[1])" }
      $restore = Run $Exe @("broker", "revert-acl")
      if ($restore[0] -ne 0) { throw "device restoration refused: $($restore[1])" }
      Say "device: $($restore[1])"
      if (Test-Path (Join-Path $Data "devices.txt")) { throw "device restoration incomplete; baseline retained for retry" }
    } catch {
      $failure = $_
      if ($wasRunning) {
        try {
          Start-Service -Name $Name
          (Get-Service -Name $Name).WaitForStatus("Running", (New-TimeSpan -Seconds 15))
        } catch { throw "uninstall refused; service recovery failed: $_; original failure: $failure" }
      }
      throw "uninstall refused; service/protection/binary retained: $failure"
    }
    if ($svc) {
      $deleted = Run sc.exe @("delete", $Name)
      if ($deleted[0] -ne 0) { throw "service deletion failed: $($deleted[1])" }
    }
    if (Test-Path (Join-Path $Before "complete")) {
      foreach ($f in $LegacyFiles) {
        $p = Join-Path $Before $f
        if (Test-Path $p) {
          $restored = Join-Path $Dir $f
          Copy-Item -Force $p $restored
          Secure-Path $restored $false $true
        }
      }
      $xml = Join-Path $Before "task.xml"
      if (Test-Path $xml) {
        Register-ScheduledTask -TaskName $OldTask -Xml ([IO.File]::ReadAllText($xml)) -Force | Out-Null
      }
      Remove-Item -Recurse -Force $Before
    }
    if (Test-Path $Exe) {
      try { Remove-Item -Force $Exe }
      catch { Rename-Item -Force $Exe ("sv-guest.exe.old-" + (Get-Date -Format "yyyyMMddHHmmss")) }
    }
    Get-ChildItem -Path $Dir -Filter "sv-guest.exe.old-*" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
    if ((Test-Path $Dir) -and -not (Get-ChildItem -Force $Dir | Select-Object -First 1)) {
      Remove-Item -Force $Dir
    }
    Say "uninstalled: the service is gone and the devices have their descriptors from before; $Keys is kept"
  }
}
