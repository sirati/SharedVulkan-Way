<# Run only in the coordinated disposable Windows client clone. No installer,
   service/device mutation, compilation, input injection or screenshots. #>
param(
  [Parameter(Mandatory=$true)][string]$Root,
  [switch]$RunRingClient,
  [switch]$RunRenderedClient,
  [string]$BarRun = '',
  [uint32]$ArenaId = 0,
  [string]$Broker = ''
)
$ErrorActionPreference = 'Stop'
$bin = Join-Path $Root 'bin'
$manifest = Get-Content -Raw -LiteralPath (Join-Path $bin 'manifest.json') | ConvertFrom-Json
foreach ($entry in $manifest.artifacts.PSObject.Properties) {
  $file = Join-Path $bin $entry.Name
  if ((Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLowerInvariant() -ne $entry.Value.sha256) {
    throw "staged artifact differs: $($entry.Name)"
  }
}
$evidence = Join-Path $Root 'evidence'
if (Test-Path -LiteralPath $evidence) { throw 'evidence directory exists; use a new run root' }
New-Item -ItemType Directory -Path $evidence | Out-Null
foreach ($name in @('home','config','data','state','cache','runtime')) {
  New-Item -ItemType Directory -Force -Path (Join-Path $Root "state\$name") | Out-Null
}
$absoluteRoot = [IO.Path]::GetFullPath($Root)
if ($absoluteRoot -notmatch '^[A-Za-z]:\\') { throw 'run root must be an absolute drive path' }
$cygwinRoot = '/cygdrive/' + $absoluteRoot.Substring(0, 1).ToLowerInvariant() + $absoluteRoot.Substring(2).Replace('\', '/')
$env:HOME = "$cygwinRoot/state/home"
$env:XDG_CONFIG_HOME = "$cygwinRoot/state/config"
$env:XDG_DATA_HOME = "$cygwinRoot/state/data"
$env:XDG_STATE_HOME = "$cygwinRoot/state/state"
$env:XDG_CACHE_HOME = "$cygwinRoot/state/cache"
$env:XDG_RUNTIME_DIR = "$cygwinRoot/state/runtime"
Remove-Item Env:WAYLAND_SOCKET -ErrorAction SilentlyContinue
Remove-Item Env:WAYLAND_DISPLAY -ErrorAction SilentlyContinue
Remove-Item Env:WC_WAYLAND_RING -ErrorAction SilentlyContinue
Remove-Item Env:WC_CYGWIN_RUN -ErrorAction SilentlyContinue
Remove-Item Env:WC_CYGWIN_ARENA_ID -ErrorAction SilentlyContinue
function Run-Probe([string]$name, [int]$seconds) {
  $stdout = Join-Path $evidence "$name.stdout.txt"
  $stderr = Join-Path $evidence "$name.stderr.txt"
  $p = New-Object Diagnostics.Process
  $p.StartInfo.FileName = Join-Path $bin "$name.exe"
  $p.StartInfo.WorkingDirectory = $bin
  $p.StartInfo.UseShellExecute = $false
  $p.StartInfo.RedirectStandardOutput = $true
  $p.StartInfo.RedirectStandardError = $true
  if (-not $p.Start()) { throw "$name did not start" }
  $outTask = $p.StandardOutput.ReadToEndAsync()
  $errTask = $p.StandardError.ReadToEndAsync()
  try {
    if (-not $p.WaitForExit($seconds * 1000)) {
      $p.Kill(); $p.WaitForExit()
      throw "$name exceeded its bounded runtime; own process stopped"
    }
    $code = $p.ExitCode
    if ($null -eq $code) { throw "$name returned no trustworthy exit status" }
    return [int]$code
  } finally {
    if (-not $p.HasExited) { $p.Kill(); $p.WaitForExit() }
    $outTask.Result | Set-Content -Encoding UTF8 -LiteralPath $stdout
    $errTask.Result | Set-Content -Encoding UTF8 -LiteralPath $stderr
    $p.Dispose()
  }
}
$abi = Run-Probe 'abi' 20
if ($abi -ne 0) { throw "Cygwin ABI runtime failed: $abi" }
if ((Get-Content -Raw -LiteralPath (Join-Path $evidence 'abi.stdout.txt')) -notmatch 'Cygwin LP64 runtime') { throw 'ABI probe did not print its runtime stamp' }
$fd = Run-Probe 'fd-probe' 20
if ($fd -ne 0 -and $fd -ne 2) { throw "Cygwin fd diagnostic failed unexpectedly: $fd" }
$fdText = Get-Content -Raw -LiteralPath (Join-Path $evidence 'fd-probe.stdout.txt')
if ($fdText -notmatch 'CYGWIN_FD .*valid_received_fds=([0-9]+)') { throw 'fd probe did not print its actual descriptor count' }
if (($fd -eq 0) -ne ([int]$Matches[1] -eq 1)) { throw 'fd probe status and descriptor count disagree' }
$result = [ordered]@{
  runtime = 'real Windows disposable clone'
  abi = $abi
  fd_probe_exit = $fd
  scm_rights_supported = ($fd -eq 0)
  rendered_frames_proved = $false
  ring_client_ran = $false
  rendered_client_ran = $false
}
if ($RunRingClient -and $RunRenderedClient) { throw 'select one client per bounded host connection' }
if ($RunRingClient -or $RunRenderedClient) {
  if (-not $Broker) { throw 'an explicit isolated broker endpoint is required' }
  if (-not (Test-Path -LiteralPath (Join-Path $bin 'wc_cygwin_link.dll'))) { throw 'bundle lacks native ring adapter' }
  $env:WC_BROKER = $Broker
  $env:WC_WAYLAND_RING = '1'
  if ($RunRenderedClient) {
    [uint64]$parsedRun = 0
    if (-not [uint64]::TryParse($BarRun, [ref]$parsedRun) -or $parsedRun -eq 0) { throw 'explicit nonzero BAR run identity required' }
    if ($parsedRun -eq [uint64]::MaxValue) { throw 'run identity must leave room for negative test' }
    $env:WC_CYGWIN_ARENA_ID = [string]$ArenaId
    $env:WC_CYGWIN_RUN = [string]($parsedRun + [uint64]1)
    $rejected = Run-Probe 'weston-info' 20
    $rejection = Get-Content -Raw -LiteralPath (Join-Path $evidence 'weston-info.stderr.txt')
    if ($rejected -eq 0 -or $rejection -notmatch 'Wayland BAR run/arena/geometry identity differs') {
      throw 'native client did not explicitly refuse the wrong BAR run identity'
    }
    $env:WC_CYGWIN_RUN = $BarRun
    $fallback = Run-Probe 'posix-probe' 20
    if ($fallback -ne 0) { throw "Cygwin POSIX mapping fallback failed: $fallback" }
    $client = Run-Probe 'weston-simple-shm' 90
    if ($client -ne 0) { throw "unmodified rendered Cygwin client failed: $client" }
    $text = Get-Content -Raw -LiteralPath (Join-Path $evidence 'weston-simple-shm.stderr.txt')
    if ($text -notmatch 'simple-shm exiting' -or $text -notmatch 'WC_CYGWIN_BAR_DESCRIPTOR ' -or
        $text -notmatch 'WC_CYGWIN_PUMPS_JOINED threads=2' -or
        $text -notmatch 'WC_CYGWIN_BAR_CONNECTION_CLOSED ring_dead=1' -or
        $text -notmatch 'WC_CYGWIN_BAR_TEARDOWN pools=([0-9]+) token_holds_closed=([0-9]+) extents_recycled=0') {
      throw 'rendered client lacks clean protocol/runtime teardown stamps'
    }
    if ([int]$Matches[1] -le 0 -or $Matches[1] -ne $Matches[2]) { throw 'BAR token holds did not close' }
    $result.rendered_client_ran = $true
  } else {
  $client = Run-Probe 'weston-info' 90
  if ($client -ne 0) { throw "unmodified Cygwin Wayland registry client failed: $client" }
  $text = Get-Content -Raw -LiteralPath (Join-Path $evidence 'weston-info.stdout.txt')
  if ($text -notmatch 'wl_compositor' -or $text -notmatch 'wl_shm') { throw 'client did not enumerate compositor/shm globals' }
  $result.ring_client_ran = $true
  }
}
$result | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $evidence 'result.json')
$result | ConvertTo-Json
