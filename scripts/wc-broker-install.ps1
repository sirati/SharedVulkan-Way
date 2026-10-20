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
  if ((Test-Path -LiteralPath $d) -and ((Get-Item -Force -LiteralPath $d).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
    throw "refusing reparse directory $d"
  }
  New-Item -ItemType Directory -Force -Path $d | Out-Null
  Secure-Path $d $true $UsersRead
}

# Validate provenance before changing any existing service or normalizing ACLs.
# A protected DACL alone is insufficient: a desktop owner can rewrite it.
function Assert-TrustedKeyTree([string]$p) {
  if (-not (Test-Path -LiteralPath $p)) { throw "existing broker key tree missing; trusted recovery required" }
  $item = Get-Item -Force -LiteralPath $p
  if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "untrusted broker key tree: reparse object" }
  $acl = Get-Acl -LiteralPath $p
  $owner = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value
  if ($owner -notin @('S-1-5-18', 'S-1-5-32-544') -or -not $acl.AreAccessRulesProtected) {
    throw "untrusted broker key tree owner or inheritance; trusted recovery required"
  }
  foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
    if ($rule.AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow -or
        $rule.IdentityReference.Value -notin @('S-1-5-18', 'S-1-5-32-544')) {
      throw "untrusted broker key tree access; trusted recovery required"
    }
  }
  if ($item.PSIsContainer) {
    foreach ($child in Get-ChildItem -Force -LiteralPath $p) { Assert-TrustedKeyTree $child.FullName }
  }
}

# Enrollment provenance is created only after the key action publishes a new
# object. Bind it to the file ID so an old receipt cannot bless a replacement.
function Key-FileId([string]$p) {
  $r = Run fsutil.exe @('file', 'queryfileid', $p)
  if ($r[0] -ne 0 -or $r[1] -notmatch '0x[0-9a-fA-F]+') { throw 'cannot identify enrolled key object' }
  return $Matches[0].ToLowerInvariant()
}
function Assert-KeyEnrollments {
  $records = Join-Path $Data 'key-enrollment'
  Assert-TrustedKeyTree $records
  foreach ($key in Get-ChildItem -Force -LiteralPath $Keys) {
    if ($key.PSIsContainer -or $key.Name -notmatch '^([A-Za-z0-9_-]{1,32})\.key$') { throw 'unknown broker key object; trusted reenrollment required' }
    $record = Join-Path $records ($Matches[1] + '.json')
    if (-not (Test-Path -LiteralPath $record)) { throw 'unknown broker key provenance; trusted reenrollment required' }
    Assert-TrustedKeyTree $record
    $r = Get-Content -Raw -LiteralPath $record | ConvertFrom-Json
    if ($r.version -ne 1 -or $r.install -cne $key.BaseName -or $r.generation -notmatch '^[a-f0-9]{32}$' -or $r.fileId -cne (Key-FileId $key.FullName)) {
      throw 'unknown broker key provenance; trusted reenrollment required'
    }
  }
}
function Record-KeyEnrollment([string]$p, [string]$id) {
  $records = Join-Path $Data 'key-enrollment'
  Secure-Dir $records $false
  $record = Join-Path $records ($id + '.json')
  $tmp = Join-Path $records ([Guid]::NewGuid().ToString('N') + '.tmp')
  try {
    $r = @{version=1; install=$id; generation=[Guid]::NewGuid().ToString('N'); fileId=(Key-FileId $p)} | ConvertTo-Json -Compress
    Write-ProtectedNewText $tmp $r
    Secure-Path $tmp $false $false
    if (Test-Path -LiteralPath $record) { Remove-Item -Force -LiteralPath $record }
    Move-Item -LiteralPath $tmp -Destination $record
  } finally { if (Test-Path -LiteralPath $tmp) { Remove-Item -Force -LiteralPath $tmp } }
}

# Object security must be supplied at creation. Tightening an old object's
# DACL cannot revoke desktop handles opened before installation.
function New-ProtectedDirectory([string]$p) {
  if (-not ('WcAtomicDirectory' -as [type])) {
    Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class WcAtomicDirectory {
  [StructLayout(LayoutKind.Sequential)] struct SECURITY_ATTRIBUTES { public int Length; public IntPtr Descriptor; public int Inherit; }
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
  static extern bool CreateDirectory(string path, ref SECURITY_ATTRIBUTES attributes);
  public static void Create(string path, byte[] descriptor) {
    IntPtr p=Marshal.AllocHGlobal(descriptor.Length);
    try {
      Marshal.Copy(descriptor,0,p,descriptor.Length);
      var a=new SECURITY_ATTRIBUTES { Length=Marshal.SizeOf(typeof(SECURITY_ATTRIBUTES)), Descriptor=p };
      if (!CreateDirectory(path, ref a)) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
    } finally { Marshal.FreeHGlobal(p); }
  }
}
'@
  }
  $a = New-Object Security.AccessControl.DirectorySecurity
  $a.SetSecurityDescriptorSddlForm('O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)')
  # CreateDirectoryW fails on an existing name, unlike Directory.CreateDirectory.
  [WcAtomicDirectory]::Create($p, $a.GetSecurityDescriptorBinaryForm())
}
function Open-ProtectedNewFile([string]$p) {
  $a = New-Object Security.AccessControl.FileSecurity
  $a.SetSecurityDescriptorSddlForm('O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)')
  return [IO.FileStream]::new($p, [IO.FileMode]::CreateNew, [Security.AccessControl.FileSystemRights]::Write, [IO.FileShare]::None, 4096, [IO.FileOptions]::None, $a)
}
function Write-ProtectedNewText([string]$p, [string]$text) {
  $s = Open-ProtectedNewFile $p
  try { $b = [Text.Encoding]::UTF8.GetBytes($text); $s.Write($b,0,$b.Length); $s.Flush($true) } finally { $s.Dispose() }
}
function Assert-DataAnchor([string]$Child = $Data) {
  # Resolve only unambiguous local DOS paths. Relative/device/UNC paths, ADS,
  # DOS aliases and Win32-normalized components cannot name a trusted anchor.
  if ($Child -notmatch '^[A-Za-z]:\\[^\\]' -or $Child.Contains('/') -or
      -not [string]::Equals([IO.Path]::GetFullPath($Child),$Child,[StringComparison]::OrdinalIgnoreCase)) { throw 'ambiguous or nonlocal broker anchor path' }
  $parts=$Child.Substring(3).Split([char]'\')
  foreach($part in $parts) {
    if (-not $part -or $part -in @('.','..') -or $part.EndsWith('.') -or $part.EndsWith(' ') -or
        $part.IndexOfAny([IO.Path]::GetInvalidFileNameChars()) -ge 0 -or $part -match '~[0-9]' -or $part -match '^(CON|PRN|AUX|NUL|CONIN\$|CONOUT\$|CLOCK\$|COM[1-9]|LPT[1-9])(\.|$)') { throw 'ambiguous broker anchor component' }
  }
  $root=$Child.Substring(0,3)
  if ([IO.DriveInfo]::new($root).DriveType -ne [IO.DriveType]::Fixed) { throw 'broker anchor must be on a local fixed drive' }
  if (-not ('WcDriveAnchor' -as [type])) {
    Add-Type @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class WcDriveAnchor {
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
  static extern uint QueryDosDevice(string name, StringBuilder target, int length);
  public static bool IsVolume(string drive) {
    var b=new StringBuilder(32768);
    if(QueryDosDevice(drive,b,b.Capacity)==0) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
    return System.Text.RegularExpressions.Regex.IsMatch(b.ToString(), @"^\\Device\\HarddiskVolume[0-9]+$", System.Text.RegularExpressions.RegexOptions.IgnoreCase);
  }
}
'@
  }
  if (-not [WcDriveAnchor]::IsVolume($root.Substring(0,2))) { throw 'ambiguous mapped broker anchor drive' }
  $privileged=@('S-1-5-18','S-1-5-32-544','S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464')
  $danger=0x10000000 -bor [Security.AccessControl.FileSystemRights]::DeleteSubdirectoriesAndFiles -bor [Security.AccessControl.FileSystemRights]::ChangePermissions -bor [Security.AccessControl.FileSystemRights]::TakeOwnership -bor [Security.AccessControl.FileSystemRights]::Delete
  # Check root-to-leaf. Only descend after the preceding object is known not
  # to permit desktop replacement, so no untrusted ancestor can swap it later.
  $ancestor=$root
  for($i=0; $i -lt $parts.Length; $i++) {
    $item=Get-Item -Force -LiteralPath $ancestor
    if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'untrusted broker anchor ancestor reparse or type' }
    $a=Get-Acl -LiteralPath $ancestor
    if ($a.GetOwner([Security.Principal.SecurityIdentifier]).Value -notin $privileged) { throw 'untrusted broker anchor ancestor owner' }
    # A verified physical volume root has no parent directory entry to
    # delete or rename. DELETE alone is inapplicable there; DELETE_CHILD and
    # ACL/owner changes remain dangerous. Every nonroot ancestor keeps DELETE.
    $applicableDanger=$danger
    if($i -eq 0) { $applicableDanger=$danger -band (-bnot [int][Security.AccessControl.FileSystemRights]::Delete) }
    foreach($r in $a.GetAccessRules($true,$true,[Security.Principal.SecurityIdentifier])) {
      if ($r.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow -and
          -not ($r.PropagationFlags -band [Security.AccessControl.PropagationFlags]::InheritOnly) -and
          $r.IdentityReference.Value -notin $privileged -and ($r.FileSystemRights -band $applicableDanger)) { throw 'untrusted broker data anchor grants replacement rights on ancestor' }
    }
    if($i -lt $parts.Length-1) { $ancestor=Join-Path $ancestor $parts[$i] }
  }
}
function Assert-DataGeneration {
  Assert-DataAnchor
  if (-not (Test-Path -LiteralPath $Data)) { throw 'broker data generation missing; fresh installation required' }
  $item=Get-Item -Force -LiteralPath $Data
  if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'untrusted broker data reparse object' }
  # Check only this object's metadata. Logs and retired recovery objects do not
  # establish provenance and must never be adopted as live key objects.
  $a=Get-Acl -LiteralPath $Data
  if (-not $a.AreAccessRulesProtected -or $a.GetOwner([Security.Principal.SecurityIdentifier]).Value -notin @('S-1-5-18','S-1-5-32-544')) { throw 'untrusted broker data generation metadata' }
  foreach($r in $a.GetAccessRules($true,$true,[Security.Principal.SecurityIdentifier])) {
    if($r.AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow -or $r.IdentityReference.Value -notin @('S-1-5-18','S-1-5-32-544')) { throw 'untrusted broker data generation access' }
  }
  $record=Join-Path $Data 'data-generation.json'
  Assert-TrustedKeyTree $record
  $g=Get-Content -Raw -LiteralPath $record | ConvertFrom-Json
  if($g.version -ne 1 -or $g.generation -notmatch '^[a-f0-9]{32}$' -or $g.fileId -cne (Key-FileId $Data)) { throw 'unknown broker data generation; fresh installation required' }
}
function Prepare-InstallKeys([bool]$ExistingService) {
  if ($ExistingService) {
    Assert-DataGeneration
    Assert-TrustedKeyTree $Keys
    Assert-KeyEnrollments
    return
  }
  Assert-DataAnchor
  if ((Test-Path -LiteralPath $Data) -and ((Get-Item -Force -LiteralPath $Data).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'refusing reparse broker data directory' }
  foreach ($history in @("devices.txt","devices.secured","pre-broker","raw-device-owners","raw-device-owners.txt")) {
    if (Test-Path -LiteralPath (Join-Path $Data $history)) { throw "prior raw security history requires trusted recovery before data replacement" }
  }
  $parent=Split-Path -Parent $Data
  $stage=Join-Path $parent ('waycompose-new-' + [Guid]::NewGuid().ToString('N'))
  New-ProtectedDirectory $stage
  # Retain the entire old tree, including logs, baselines and old enrollment
  # records. Old parent/file handles can mutate only this retired generation.
  if(Test-Path -LiteralPath $Data) {
    $recovery=Join-Path $parent ('waycompose-recovery-' + [Guid]::NewGuid().ToString('N'))
    New-ProtectedDirectory $recovery
    [IO.Directory]::Move($Data, (Join-Path $recovery 'retired-data'))
    # Never traverse or normalize retired objects: their retained handles
    # can still rewrite ACLs or install reparse points inside the old tree.
    Say 'prior data generation isolated in privileged recovery; fresh protected enrollment required'
  }
  # A racing fixed-name creation fails closed. Never normalize or adopt it.
  [IO.Directory]::Move($stage, $Data)
  New-ProtectedDirectory $Keys
  New-ProtectedDirectory (Join-Path $Data 'key-enrollment')
  $g=@{version=1; generation=[Guid]::NewGuid().ToString('N'); fileId=(Key-FileId $Data)} | ConvertTo-Json -Compress
  Write-ProtectedNewText (Join-Path $Data 'data-generation.json') $g
}

function Prepare-BinaryDirectory {
  Assert-DataAnchor $Dir
  if (Test-Path -LiteralPath $Dir) {
    $item=Get-Item -Force -LiteralPath $Dir
    if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'untrusted broker binary directory reparse or type' }
    $a=Get-Acl -LiteralPath $Dir
    if (-not $a.AreAccessRulesProtected -or $a.GetOwner([Security.Principal.SecurityIdentifier]).Value -notin @('S-1-5-18','S-1-5-32-544')) { throw 'untrusted broker binary directory metadata; trusted recovery required' }
    $write=[Security.AccessControl.FileSystemRights]::Write -bor [Security.AccessControl.FileSystemRights]::Delete -bor [Security.AccessControl.FileSystemRights]::DeleteSubdirectoriesAndFiles -bor [Security.AccessControl.FileSystemRights]::ChangePermissions -bor [Security.AccessControl.FileSystemRights]::TakeOwnership
    foreach($r in $a.GetAccessRules($true,$true,[Security.Principal.SecurityIdentifier])) {
      if($r.AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow -or
        ($r.IdentityReference.Value -notin @('S-1-5-18','S-1-5-32-544') -and ($r.FileSystemRights -band $write))) { throw 'untrusted broker binary directory access' }
    }
    $record=Join-Path $Dir 'binary-generation.json'
    Assert-TrustedKeyTree $record
    $g=Get-Content -Raw -LiteralPath $record | ConvertFrom-Json
    if($g.version -ne 1 -or $g.generation -notmatch '^[a-f0-9]{32}$' -or $g.fileId -cne (Key-FileId $Dir)) { throw 'unknown broker binary directory generation; trusted recovery required' }
    return
  }
  New-ProtectedDirectory $Dir
  $g=@{version=1;generation=[Guid]::NewGuid().ToString('N');fileId=(Key-FileId $Dir)} | ConvertTo-Json -Compress
  Write-ProtectedNewText (Join-Path $Dir 'binary-generation.json') $g
  Secure-Path $Dir $true $true
}
function Copy-ProtectedBinary([string]$source, [string]$destination) {
  $inputStream=[IO.File]::OpenRead($source)
  try {
    $outputStream=Open-ProtectedNewFile $destination
    try { $inputStream.CopyTo($outputStream); $outputStream.Flush($true) } finally { $outputStream.Dispose() }
    Secure-Path $destination $false $true
  } finally { $inputStream.Dispose() }
}

switch ($Action) {
  "install" {
    if (-not $Source) { throw "-Source is required" }
    $src = Join-Path $Source "sv-guest.exe"
    if (-not (Test-Path $src)) { throw "no $src" }
    $svc = Get-Service -Name $Name -ErrorAction SilentlyContinue
    Prepare-BinaryDirectory
    Prepare-InstallKeys ([bool]$svc)
    Secure-Dir $Dir $true
    Secure-Dir $Data $false
    Secure-Dir $Keys $false
    $baseline = Join-Path $Data "devices.txt"
    if (-not (Test-Path $baseline)) {
      if ($svc -or (Test-Path (Join-Path $Before "complete")) -or (Test-Path (Join-Path $Data "devices.secured"))) {
        throw "lost device baseline; trusted recovery required before changing the service"
      }
      [IO.File]::WriteAllBytes($baseline, [byte[]]@())
      Secure-Path $baseline $false $false
    }
    if ($svc -and $svc.Status -ne "Stopped") {
      Stop-Service -Name $Name -Force -ErrorAction SilentlyContinue
      (Get-Service -Name $Name).WaitForStatus("Stopped", (New-TimeSpan -Seconds 15))
    }
    Get-ChildItem -Path $Dir -Filter "sv-guest.exe.old-*" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
    if (Test-Path $Exe) {
      try { Remove-Item -Force $Exe } catch { Rename-Item -Force $Exe ("sv-guest.exe.old-" + (Get-Date -Format "yyyyMMddHHmmss")) }
    }
    Copy-ProtectedBinary $src $Exe
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
    # SCM Running alone precedes asynchronous startup checks. Require the
    # public broker to answer and independently verify current raw protection.
    $until = [DateTime]::UtcNow.AddSeconds(15)
    do {
      $ready = Run $Exe @("broker", "ready")
      if ($ready[0] -eq 0) { break }
      Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $until)
    if ($ready[0] -ne 0) { throw "broker installation failed readiness/protection verification: $($ready[1])" }
    $log = Join-Path $Data "broker.log"
    if (Test-Path $log) { Get-Content -Tail 12 $log | ForEach-Object { Say "log: $_" } }
    Say "installed: $Exe as service $Name (LocalSystem, automatic, restarted on failure)"
  }
  "key" {
    if ($Install -notmatch '^[A-Za-z0-9_-]{1,32}$') { throw "-Install must be an installation name" }
    $k = (@($input) -join "").Trim()
    if (-not $k) { $k = [Console]::In.ReadToEnd().Trim() }
    if ($k -notmatch '^[0-9a-f]{64}$') { throw "the key on stdin is not 64 hex digits" }
    Assert-DataGeneration
    Assert-TrustedKeyTree $Keys
    Assert-TrustedKeyTree (Join-Path $Data "key-enrollment")
    $p = Join-Path $Keys "$Install.key"
    if ((Test-Path -LiteralPath $p) -and ((Get-Item -Force -LiteralPath $p).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
      throw "refusing reparse key $p"
    }
    # Replace the object, never overwrite a formerly exposed key in place.
    # A desktop process may still have the old object open with read sharing;
    # changing its DACL cannot revoke that handle's access to new bytes.
    $tmp = Join-Path $Keys (".key-" + [Guid]::NewGuid().ToString("N") + ".tmp")
    try {
      $stream = Open-ProtectedNewFile $tmp
      try {
        $bytes = [Text.Encoding]::ASCII.GetBytes($k)
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush($true)
      } finally { $stream.Dispose() }
      Secure-Path $tmp $false $false
      # A failure leaves the signer unavailable rather than publishing to an
      # existing object. The protected parent prevents desktop replacement.
      if (Test-Path -LiteralPath $p) { Remove-Item -Force -LiteralPath $p }
      Move-Item -LiteralPath $tmp -Destination $p
    } finally {
      if (Test-Path -LiteralPath $tmp) { Remove-Item -Force -LiteralPath $tmp }
    }
    Record-KeyEnrollment $p $Install
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
    # Saved legacy grant code can reopen raw devices after a safe descriptor
    # restoration. Its old task or script is not a reviewed drain boundary.
    # Refuse before touching the service, even with a restricted raw baseline.
    foreach ($legacy in @("task.xml", "wc-port-access.ps1", "ports.json")) {
      if (Test-Path -LiteralPath (Join-Path $Before $legacy)) {
        throw "uninstall refused before service mutation: unreviewed legacy grant restoration ($legacy); retain broker protections and use protected application rollback"
      }
    }
    # Do not stop a working broker when rollback would expose uncleared BARs.
    if (-not (Test-Path $Exe)) { throw "broker executable missing; cannot prove safe uninstall" }
    $check = Run $Exe @("broker", "revert-acl-check")
    if ($check[0] -ne 0) { throw "uninstall refused before service mutation: $($check[1])" }
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
      if ((Test-Path (Join-Path $Data "devices.txt")) -or (Test-Path (Join-Path $Data "devices.secured"))) {
        throw "device restoration incomplete; baseline or checkpoint retained for trusted recovery"
      }
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
