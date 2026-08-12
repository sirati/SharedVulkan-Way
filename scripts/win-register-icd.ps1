# Register (or unregister) the shared_vulkan ICD with the Windows Vulkan loader.
#
# The Windows half of design.md §8. It matters only when the *app side* is
# Windows — not on testhost, where the app side is the Linux host — but the
# brief requires either side to be either OS, so the path exists and is written
# down rather than described.
#
#   win-register-icd.ps1 -Manifest C:\path\to\sv_icd.json          # register
#   win-register-icd.ps1 -Manifest C:\path\to\sv_icd.json -Remove  # undo
#   win-register-icd.ps1 -List                                     # show only
#
# Why the registry and not a file in a directory, as on Linux: the Windows
# loader has no `icd.d`. It enumerates
# `HKLM\SOFTWARE\Khronos\Vulkan\Drivers`, where each *value name* is the full
# path to a manifest and the value itself is a `DWORD` of 0 meaning "enabled".
# The loader documentation names this "the primary location for software based
# drivers"; it needs administrator rights but **no signing, no PnP device and no
# WHQL**, and SwiftShader registers exactly this way. That is what makes an
# unsigned software driver installable at all, and it is why `security-model.md`
# R6's "no permanent test-signing" constraint costs this project nothing.
#
# The `WOW6432Node` mirror is for 32-bit applications on a 64-bit system. A
# 64-bit driver must NOT be registered there — a 32-bit loader would try to load
# a 64-bit DLL — so this script writes it only when given a 32-bit manifest.
#
# The development-loop alternative, which needs no administrator at all, is the
# environment variable:
#
#   $env:VK_DRIVER_FILES = "C:\path\to\sv_icd.json"
#
# `VK_DRIVER_FILES` and not `VK_ICD_FILENAMES`: the latter is deprecated and the
# loader ignores it when both are set.

param(
  [string]$Manifest,
  [switch]$Remove,
  [switch]$List,
  [ValidateSet('64', '32')][string]$Arch = '64'
)
$ErrorActionPreference = 'Stop'

$key64 = 'HKLM:\SOFTWARE\Khronos\Vulkan\Drivers'
$key32 = 'HKLM:\SOFTWARE\WOW6432Node\Khronos\Vulkan\Drivers'
$key = if ($Arch -eq '32') { $key32 } else { $key64 }

function Show-Drivers {
  foreach ($k in @($key64, $key32)) {
    if (Test-Path $k) {
      Write-Host "$k"
      $item = Get-Item $k
      foreach ($name in $item.GetValueNames()) {
        $v = $item.GetValue($name)
        $state = if ($v -eq 0) { 'enabled' } else { "disabled ($v)" }
        Write-Host "  $name  [$state]"
      }
    } else {
      Write-Host "$k  (absent)"
    }
  }
}

if ($List) { Show-Drivers; exit 0 }

if (-not $Manifest) {
  throw "give -Manifest with the full path to sv_icd.json, or -List"
}

# The loader takes the value *name* as a path and does not resolve it, so a
# relative path registers a driver that can never be found.
$full = [System.IO.Path]::GetFullPath($Manifest)

if ($Remove) {
  if (Test-Path $key) {
    Remove-ItemProperty -Path $key -Name $full -ErrorAction SilentlyContinue
    Write-Host "removed $full from $key"
  }
  exit 0
}

if (-not (Test-Path $full)) {
  throw "no manifest at $full — build sv-icd first; its build script writes one next to the DLL"
}

# Fail early and clearly rather than after a half-written registry change.
$json = Get-Content -Raw $full | ConvertFrom-Json
if (-not $json.ICD.library_path) {
  throw "$full does not look like an ICD manifest (no ICD.library_path)"
}
if ($json.ICD.api_version -notmatch '^1\.[1-9]') {
  # design.md §8: below 1.1 the loader demotes the driver to Vulkan 1.0 *and
  # rewrites every application's requested apiVersion*, with no diagnostic.
  throw "manifest api_version is $($json.ICD.api_version); below 1.1 the loader silently demotes the driver"
}

New-Item -Path $key -Force | Out-Null
New-ItemProperty -Path $key -Name $full -PropertyType DWord -Value 0 -Force | Out-Null
Write-Host "registered $full in $key"
Show-Drivers
