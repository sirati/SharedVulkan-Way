<# Run only in the coordinator-assigned disposable Windows fixture.
   No broker/task/driver/registry mutation or user-input injection. #>
param([Parameter(Mandatory=$true)][string]$Root)
$ErrorActionPreference='Stop'
if($Root -notmatch '^[A-Za-z]:\\[A-Za-z0-9_-]+$'){throw 'use a short absolute drive run root without spaces'}
$e=Join-Path $Root 'evidence'
if(Test-Path -LiteralPath $e){throw 'use a fresh run root'}
New-Item -ItemType Directory -Path $e | Out-Null
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class OwnedLocalWindow {
 [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd,out uint pid);
 [DllImport("user32.dll",SetLastError=true)] static extern bool PostMessageW(IntPtr hwnd,uint msg,UIntPtr wp,IntPtr lp);
 public static void Close(ulong raw,uint expected) {
  uint pid;var hwnd=new IntPtr(unchecked((long)raw));
  if(GetWindowThreadProcessId(hwnd,out pid)==0||pid!=expected)throw new Exception("window is not owned by this test compositor");
  if(!PostMessageW(hwnd,0x0010,UIntPtr.Zero,IntPtr.Zero))throw new Exception("owned window close failed");
 }
}
'@
function Start-Owned([string]$file,[string[]]$arguments,[string]$label) {
 $p=New-Object Diagnostics.Process
 $p.StartInfo.FileName=$file
 $p.StartInfo.Arguments=[string]::Join(' ',$arguments)
 $p.StartInfo.WorkingDirectory=Split-Path -Parent $file
 $p.StartInfo.UseShellExecute=$false
 $p.StartInfo.RedirectStandardOutput=$true
 $p.StartInfo.RedirectStandardError=$true
 if(-not $p.Start()){throw "$label did not start"}
 return @{p=$p;out=$p.StandardOutput.ReadToEndAsync();err=$p.StandardError.ReadToEndAsync();label=$label}
}
function Save-Owned($o){
 $o.out.Result | Set-Content -Encoding UTF8 -LiteralPath (Join-Path $e ($o.label+'.stdout'))
 $o.err.Result | Set-Content -Encoding UTF8 -LiteralPath (Join-Path $e ($o.label+'.stderr'))
}
function Wait-Owned($o,[int]$seconds){
 if(-not $o.p.WaitForExit($seconds*1000)){throw ($o.label+' timed out')}
 Save-Owned $o
 if($null -eq $o.p.ExitCode -or $o.p.ExitCode -ne 0){throw ($o.label+' failed: '+$o.p.ExitCode)}
}
function Set-State([string]$kind,[bool]$cygwin){
 foreach($part in @('home','config','data','state','cache','runtime','local')) {New-Item -ItemType Directory -Force -Path (Join-Path $Root "state\$kind\$part")|Out-Null}
 $base=Join-Path $Root "state\$kind"
 if($cygwin){$base='/cygdrive/'+$base.Substring(0,1).ToLowerInvariant()+$base.Substring(2).Replace('\','/')}
 $env:HOME="$base/home";$env:XDG_CONFIG_HOME="$base/config";$env:XDG_DATA_HOME="$base/data"
 $env:XDG_STATE_HOME="$base/state";$env:XDG_CACHE_HOME="$base/cache"
 $runtime=$privateRuntime[$kind]
 if($runtime){
  if($cygwin){$runtime='/cygdrive/'+$runtime.Substring(0,1).ToLowerInvariant()+$runtime.Substring(2).Replace('\','/')}
  $env:XDG_RUNTIME_DIR=$runtime
 }else{$env:XDG_RUNTIME_DIR="$base/runtime"}
 $env:WC_LOCAL_PRIVATE='1'
 $env:LOCALAPPDATA=Join-Path $Root "state\$kind\local"
 foreach($name in @('WC_BROKER','WC_WAYLAND_RING','WC_CYGWIN_RUN','WC_CYGWIN_ARENA_ID','WAYLAND_SOCKET','SV_SOCKET','SV_BELL')){Remove-Item "Env:$name" -ErrorAction SilentlyContinue}
}
$results=@()
$privateRuntime=@{}
foreach($kind in @('mingw','cygwin')){
 $bin=Join-Path $Root $kind
 $manifest=Get-Content -Raw -LiteralPath (Join-Path $bin 'manifest.json')|ConvertFrom-Json
 foreach($entry in $manifest.artifacts.PSObject.Properties){
  if((Get-FileHash -LiteralPath (Join-Path $bin $entry.Name) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $entry.Value.sha256){throw ('artifact differs: '+$kind+'/'+$entry.Name)}
 }
 foreach($entry in $manifest.generated_sources.PSObject.Properties){
  if((Get-FileHash -LiteralPath (Join-Path $bin $entry.Name) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $entry.Value){throw ('source receipt differs: '+$kind+'/'+$entry.Name)}
 }
 Set-State $kind $false
 $setup=Start-Owned (Join-Path $bin 'wc-local-namespace.exe') @($env:XDG_RUNTIME_DIR) "$kind-namespace"
 try {Wait-Owned $setup 15} finally {if(-not $setup.p.HasExited){$setup.p.Kill();$setup.p.WaitForExit()};Save-Owned $setup;$setup.p.Dispose()}
 $runtime=(Get-Content -Raw (Join-Path $e "$kind-namespace.stdout")).Trim()
 if($runtime -notmatch '^[A-Za-z]:\\' -or [Text.Encoding]::UTF8.GetByteCount($runtime+'/local-'+$kind+'-shm') -ge 108){throw 'invalid or overlong private runtime'}
 $privateRuntime[$kind]=$runtime
 Set-State $kind ($kind -eq 'cygwin')
 $probe=Start-Owned (Join-Path $bin 'abi.exe') @() "$kind-abi"
 try {Wait-Owned $probe 15} finally {if(-not $probe.p.HasExited){$probe.p.Kill();$probe.p.WaitForExit()};Save-Owned $probe;$probe.p.Dispose()}
 $abi=Get-Content -Raw (Join-Path $e "$kind-abi.stdout")
 if($kind -eq 'mingw' -and $abi -notmatch 'MinGW LLP64 runtime'){throw 'native LLP64 stamp absent'}
 if($kind -eq 'cygwin' -and $abi -notmatch 'Cygwin LP64 runtime'){throw 'Cygwin LP64 stamp absent'}
 $probe=Start-Owned (Join-Path $bin 'posix-probe.exe') @() "$kind-posix"
 try {Wait-Owned $probe 15} finally {if(-not $probe.p.HasExited){$probe.p.Kill();$probe.p.WaitForExit()};Save-Owned $probe;$probe.p.Dispose()}
 if((Get-Content -Raw (Join-Path $e "$kind-posix.stdout")) -notmatch 'LOCAL_SECTION_POSIX_OK .*zero_retained=1'){throw 'local section/alias/read-only/lifetime contract absent'}
 foreach($mode in @('info','shm')){
  $serverOwned=$null;$clientOwned=$null
  $log=Join-Path $e "$kind-$mode-server.log"
  $frames=Join-Path $e "$kind-$mode-frames"
  $env:WAYLAND_DISPLAY="local-$kind-$mode"
  try {
   Set-State $kind $false
   $arguments=@('--socket',$env:WAYLAND_DISPLAY,'--timeout','35','--once','--log',$log)
   if($mode -eq 'shm'){$arguments+=@('--expect-presents','12','--dump-frames',$frames)}
   $serverOwned=Start-Owned (Join-Path $Root 'wc-compositor.exe') $arguments "$kind-$mode-server"
   $ready=$false
   for($i=0;$i-lt 100;$i++){
    if($serverOwned.p.HasExited){throw 'local compositor exited before socket readiness'}
    if((Test-Path $log) -and (Get-Content -Raw $log) -match "listening on local-$kind-$mode"){$ready=$true;break}
    Start-Sleep -Milliseconds 100
   }
   if(-not $ready){throw 'local socket readiness timed out'}
   Set-State $kind ($kind -eq 'cygwin')
   $clientName=if($mode -eq 'info'){'weston-info.exe'}else{'simple-shm.exe'}
   $clientOwned=Start-Owned (Join-Path $bin $clientName) @() "$kind-$mode-client"
   if($mode -eq 'shm'){
    $painted=$false
    for($i=0;$i-lt 200;$i++){
     if($clientOwned.p.HasExited){throw 'ordinary application exited before 12 frames'}
     if((Get-ChildItem -LiteralPath $frames -Filter '*.ppm' -ErrorAction SilentlyContinue|Measure-Object).Count -ge 12){$painted=$true;break}
     Start-Sleep -Milliseconds 100
    }
    if(-not $painted){throw 'ordinary application did not present 12 frames'}
    $serverText=Get-Content -Raw $log
    if($serverText -notmatch 'created \(HWND 0x([0-9a-fA-F]+)\)'){throw 'owned HWND absent'}
    $hwnd=[Convert]::ToUInt64($Matches[1],16)
    [OwnedLocalWindow]::Close($hwnd,[uint32]$serverOwned.p.Id)
   }
   Wait-Owned $clientOwned 15
   Wait-Owned $serverOwned 15
   $serverText=Get-Content -Raw $log
   if($mode -eq 'info'){
    $text=Get-Content -Raw (Join-Path $e "$kind-$mode-client.stdout")
    if($text -notmatch 'wl_compositor' -or $text -notmatch 'wl_shm'){throw 'upstream registry enumeration absent'}
   }else{
    $text=Get-Content -Raw (Join-Path $e "$kind-$mode-client.stderr")
    if($text -notmatch 'simple-shm exiting'){throw 'unchanged application did not close normally'}
    if($serverText -notmatch 'pixels served by wl_shm.*[1-9][0-9]* section, 0 shared-file, 0 arena-slice'){throw 'original named-section pixels absent'}
    if($serverText -notmatch 'drawn directly from its client'){throw 'direct original-buffer presentation absent'}
    $hashes=@(Get-ChildItem -LiteralPath $frames -Filter '*.ppm'|Get-FileHash -Algorithm SHA256|Select-Object -ExpandProperty Hash -Unique)
    if($hashes.Count -lt 3){throw 'fewer than three distinct presented pixel frames'}
    if($serverText -notmatch '0 still held, 0 still alive, 0 client'){throw 'native window/client teardown incomplete'}
    $results+=@{abi=$kind;ordinary_source_unmodified=$true;named_section=$true;broker_dependency=$false;vm_bar_dependency=$false;distinct_frames=$hashes.Count;normal_close=$true}
   }
  }finally{
   foreach($owned in @($clientOwned,$serverOwned)){if($owned){if(-not $owned.p.HasExited){$owned.p.Kill();$owned.p.WaitForExit()};Save-Owned $owned;$owned.p.Dispose()}}
  }
 }
}
$results|ConvertTo-Json -Depth 5|Set-Content -LiteralPath (Join-Path $e 'result.json')
Write-Output 'LOCAL_ORDINARY_CYGWIN_MINGW_REAL_WINDOWS_PASS'
