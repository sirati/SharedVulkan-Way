<# Run only in the coordinated disposable clone, with a two-client host relay
   and an explicit --allow-data-control compositor. No installation or mutation. #>
param(
  [Parameter(Mandatory=$true)][string]$Root,
  [Parameter(Mandatory=$true)][string]$BarRun,
  [Parameter(Mandatory=$true)][uint32]$ArenaId,
  [Parameter(Mandatory=$true)][string]$Broker,
  [string]$LegacyBin = ''
)
$ErrorActionPreference = 'Stop'
$bin = Join-Path $Root 'bin'
$evidence = Join-Path $Root 'evidence'
if (Test-Path $evidence) { throw 'evidence exists; use a new run root' }
New-Item -ItemType Directory $evidence | Out-Null
$manifest = Get-Content -Raw (Join-Path $bin 'manifest.json') | ConvertFrom-Json
foreach ($entry in $manifest.artifacts.PSObject.Properties) {
  if ((Get-FileHash (Join-Path $bin $entry.Name) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $entry.Value.sha256) { throw "artifact differs: $($entry.Name)" }
}
$absoluteRoot = [IO.Path]::GetFullPath($Root)
if ($absoluteRoot -notmatch '^[A-Za-z]:\\') { throw 'absolute drive root required' }
$cygwinRoot = '/cygdrive/' + $absoluteRoot.Substring(0,1).ToLowerInvariant() + $absoluteRoot.Substring(2).Replace('\','/')
foreach ($name in @('home','config','data','state','cache','runtime')) {
  New-Item -ItemType Directory -Force (Join-Path $Root "state\$name") | Out-Null
}
$env:HOME = "$cygwinRoot/state/home"
$env:XDG_RUNTIME_DIR = "$cygwinRoot/state/runtime"
$env:XDG_CONFIG_HOME = "$cygwinRoot/state/config"
$env:XDG_DATA_HOME = "$cygwinRoot/state/data"
$env:XDG_STATE_HOME = "$cygwinRoot/state/state"
$env:XDG_CACHE_HOME = "$cygwinRoot/state/cache"
$env:PATH = "$bin;$env:SystemRoot\System32;$env:SystemRoot"
$env:WC_BROKER = $Broker
$env:WC_WAYLAND_RING = '1'
$env:WC_CYGWIN_ARENA_ID = [string]$ArenaId
$env:WC_CYGWIN_RUN = $BarRun
$env:WAYLAND_DEBUG = '1'
Remove-Item Env:WAYLAND_SOCKET -ErrorAction SilentlyContinue
Remove-Item Env:WAYLAND_DISPLAY -ErrorAction SilentlyContinue
function Reject-Probe([string]$directory, [string]$label, [string]$pattern) {
  $p = New-Object Diagnostics.Process
  $p.StartInfo.FileName = Join-Path $directory 'weston-info.exe'
  $p.StartInfo.WorkingDirectory = $directory
  $p.StartInfo.UseShellExecute = $false
  $p.StartInfo.RedirectStandardError = $true
  $p.StartInfo.RedirectStandardOutput = $true
  if (-not $p.Start()) { throw 'negative probe did not start' }
  $errors = $p.StandardError.ReadToEndAsync()
  $stdout = $p.StandardOutput.ReadToEndAsync()
  try {
    if (-not $p.WaitForExit(20000)) { throw 'negative probe exceeded runtime' }
    $errors.Result | Set-Content -Encoding UTF8 (Join-Path $evidence "$label.stderr.txt")
    $stdout.Result | Set-Content -Encoding UTF8 (Join-Path $evidence "$label.stdout.txt")
    if ($p.ExitCode -eq 0 -or $errors.Result -notmatch $pattern) { throw "$label did not fail closed" }
  } finally {
    if (-not $p.HasExited) { $p.Kill(); $p.WaitForExit() }
    $p.Dispose()
  }
}
if ($LegacyBin) {
  $legacyManifest = Get-Content -Raw (Join-Path $LegacyBin 'manifest.json') | ConvertFrom-Json
  foreach ($entry in $legacyManifest.artifacts.PSObject.Properties) {
    if ((Get-FileHash (Join-Path $LegacyBin $entry.Name) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $entry.Value.sha256) { throw 'legacy artifact differs' }
  }
  Reject-Probe $LegacyBin 'legacy-format' 'header version 4|requires lease header format 3'
}
$env:WC_CYGWIN_RUN = [string]([uint64]$BarRun + [uint64]1)
Reject-Probe $bin 'wrong-run' 'Wayland BAR run/arena/geometry identity differs'
$env:WC_CYGWIN_RUN = $BarRun
# 26 KiB exceeds the stream's 16 KiB ring. The unchanged argv mode avoids
# wl-copy's optional stdin fork. wl-paste still execs the genuine Cygwin cat.
$payload = ('BAR_STREAM_0123456789' * 1350)
$expected = [Text.Encoding]::ASCII.GetBytes($payload)
$copy = $null
$paste = $null
$output = $null
try {
  $copy = Start-Process -FilePath (Join-Path $bin 'wl-copy.exe') -WorkingDirectory $bin -ArgumentList @('--foreground','--paste-once','--type','application/octet-stream',$payload) -PassThru -RedirectStandardOutput (Join-Path $evidence 'wl-copy.stdout.txt') -RedirectStandardError (Join-Path $evidence 'wl-copy.stderr.txt')
  # Retain the actual process handle before it can exit, so Windows PowerShell
  # can still obtain its exit status after the one-shot source completes.
  $null = $copy.Handle
  $deadline = [DateTime]::UtcNow.AddSeconds(20)
  do {
    if ($copy.HasExited) { throw "wl-copy exited before selection: $($copy.ExitCode)" }
    $trace = Get-Content -Raw (Join-Path $evidence 'wl-copy.stderr.txt') -ErrorAction SilentlyContinue
    if ($trace -match 'set_selection\(') { break }
    if ([DateTime]::UtcNow -gt $deadline) { throw 'wl-copy did not establish a selection' }
    Start-Sleep -Milliseconds 50
  } while ($true)
  $paste = New-Object Diagnostics.Process
  $paste.StartInfo.FileName = Join-Path $bin 'wl-paste.exe'
  $paste.StartInfo.Arguments = '--no-newline --type application/octet-stream'
  $paste.StartInfo.WorkingDirectory = $bin
  $paste.StartInfo.UseShellExecute = $false
  $paste.StartInfo.RedirectStandardOutput = $true
  $paste.StartInfo.RedirectStandardError = $true
  if (-not $paste.Start()) { throw 'wl-paste did not start' }
  $output = [IO.File]::Create((Join-Path $evidence 'clipboard.bin'))
  $bytesTask = $paste.StandardOutput.BaseStream.CopyToAsync($output)
  $errorTask = $paste.StandardError.ReadToEndAsync()
  if (-not $paste.WaitForExit(60000)) { throw 'wl-paste exceeded bounded runtime' }
  $bytesTask.GetAwaiter().GetResult()
  $output.Dispose(); $output = $null
  $errorTask.Result | Set-Content -Encoding UTF8 (Join-Path $evidence 'wl-paste.stderr.txt')
  if ($paste.ExitCode -ne 0) { throw "wl-paste failed: $($paste.ExitCode)" }
  if (-not $copy.WaitForExit(30000)) { throw 'wl-copy did not finish its one actual transfer' }
  if ($null -eq $copy.ExitCode -or $copy.ExitCode -ne 0) { throw "wl-copy failed or returned no exit status: $($copy.ExitCode)" }
  $actual = [IO.File]::ReadAllBytes((Join-Path $evidence 'clipboard.bin'))
  if ($actual.Length -ne $expected.Length) { throw "clipboard length differs: $($actual.Length)" }
  for ($i = 0; $i -lt $actual.Length; $i++) { if ($actual[$i] -ne $expected[$i]) { throw "clipboard differs at $i" } }
  $hash = (Get-FileHash (Join-Path $evidence 'clipboard.bin') -Algorithm SHA256).Hash.ToLowerInvariant()
  [ordered]@{bytes=$actual.Length;sha256=$hash;producer_exit=$copy.ExitCode;consumer_exit=$paste.ExitCode;upstream_version='2.3.0';protected_retirement_proved=$false} | ConvertTo-Json | Set-Content -Encoding UTF8 (Join-Path $evidence 'clipboard-result.json')
} finally {
  if ($output) { $output.Dispose() }
  foreach ($process in @($copy,$paste)) {
    if ($process) { if (-not $process.HasExited) { $process.Kill(); $process.WaitForExit() }; $process.Dispose() }
  }
}
