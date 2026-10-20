<# Run only in the coordinated disposable Windows client clone. No installer,
   service/device mutation, compilation, input injection or screenshots. #>
param(
  [Parameter(Mandatory=$true)][string]$Root,
  [switch]$RunRingClient,
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
$env:HOME = '/state/home'
$env:XDG_CONFIG_HOME = '/state/config'
$env:XDG_DATA_HOME = '/state/data'
$env:XDG_STATE_HOME = '/state/state'
$env:XDG_CACHE_HOME = '/state/cache'
$env:XDG_RUNTIME_DIR = '/state/runtime'
Remove-Item Env:WAYLAND_SOCKET -ErrorAction SilentlyContinue
Remove-Item Env:WAYLAND_DISPLAY -ErrorAction SilentlyContinue
Remove-Item Env:WC_WAYLAND_RING -ErrorAction SilentlyContinue
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
}
if ($RunRingClient) {
  if (-not $Broker) { throw 'an explicit isolated broker endpoint is required' }
  if (-not (Test-Path -LiteralPath (Join-Path $bin 'wc_cygwin_link.dll'))) { throw 'bundle lacks native ring adapter' }
  $env:WC_BROKER = $Broker
  $env:WC_WAYLAND_RING = '1'
  $client = Run-Probe 'weston-info' 90
  if ($client -ne 0) { throw "unmodified Cygwin Wayland registry client failed: $client" }
  $text = Get-Content -Raw -LiteralPath (Join-Path $evidence 'weston-info.stdout.txt')
  if ($text -notmatch 'wl_compositor' -or $text -notmatch 'wl_shm') { throw 'client did not enumerate compositor/shm globals' }
  $result.ring_client_ran = $true
}
$result | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $evidence 'result.json')
$result | ConvertTo-Json
