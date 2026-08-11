# Grant or revoke a single LSA account right, and nothing else.
#
# Used by shared_vulkan step 0 to give one account SeLockMemoryPrivilege, which
# large pages and AWE both require. LsaAddAccountRights is purely additive: it
# adds one right to one SID and touches no other policy, which is why this is
# preferred over `secedit /configure` (which applies a whole template).
# LsaRemoveAccountRights is its exact inverse.
#
#   svpriv.ps1 -Sid <SID> -Right SeLockMemoryPrivilege            # grant
#   svpriv.ps1 -Sid <SID> -Right SeLockMemoryPrivilege -Revoke    # undo
#   svpriv.ps1 -Sid <SID> -Right SeLockMemoryPrivilege -List      # show only

param(
  [Parameter(Mandatory=$true)][string]$Sid,
  [Parameter(Mandatory=$true)][string]$Right,
  [switch]$Revoke,
  [switch]$List
)
$ErrorActionPreference = 'Stop'

Add-Type @"
using System;
using System.Runtime.InteropServices;
public class SvLsa {
  [StructLayout(LayoutKind.Sequential)]
  public struct LSA_UNICODE_STRING { public ushort Length; public ushort MaximumLength; public IntPtr Buffer; }
  [StructLayout(LayoutKind.Sequential)]
  public struct LSA_OBJECT_ATTRIBUTES {
    public int Length; public IntPtr RootDirectory; public IntPtr ObjectName;
    public uint Attributes; public IntPtr SecurityDescriptor; public IntPtr SecurityQualityOfService;
  }
  [DllImport("advapi32.dll", SetLastError=true)]
  public static extern uint LsaOpenPolicy(IntPtr SystemName, ref LSA_OBJECT_ATTRIBUTES oa, uint access, out IntPtr handle);
  [DllImport("advapi32.dll", SetLastError=true)]
  public static extern uint LsaAddAccountRights(IntPtr policy, byte[] sid, LSA_UNICODE_STRING[] rights, uint count);
  [DllImport("advapi32.dll", SetLastError=true)]
  public static extern uint LsaRemoveAccountRights(IntPtr policy, byte[] sid, bool allRights, LSA_UNICODE_STRING[] rights, uint count);
  [DllImport("advapi32.dll", SetLastError=true)]
  public static extern uint LsaEnumerateAccountRights(IntPtr policy, byte[] sid, out IntPtr rights, out uint count);
  [DllImport("advapi32.dll")] public static extern uint LsaClose(IntPtr h);
  [DllImport("advapi32.dll")] public static extern uint LsaFreeMemory(IntPtr p);
  [DllImport("advapi32.dll")] public static extern int LsaNtStatusToWinError(uint status);
}
"@

function New-LsaString([string]$s) {
  $u = New-Object SvLsa+LSA_UNICODE_STRING
  $u.Buffer = [Runtime.InteropServices.Marshal]::StringToHGlobalUni($s)
  $u.Length = [System.UInt16]($s.Length * 2)
  $u.MaximumLength = [System.UInt16]($s.Length * 2 + 2)
  return $u
}

$sidBytes = New-Object byte[] 0
$sidObj = New-Object System.Security.Principal.SecurityIdentifier($Sid)
$sidBytes = New-Object byte[] $sidObj.BinaryLength
$sidObj.GetBinaryForm($sidBytes, 0)

$oa = New-Object SvLsa+LSA_OBJECT_ATTRIBUTES
$oa.Length = [Runtime.InteropServices.Marshal]::SizeOf($oa)
$policy = [IntPtr]::Zero
$POLICY_ALL_ACCESS = 0x000F0FFF
$st = [SvLsa]::LsaOpenPolicy([IntPtr]::Zero, [ref]$oa, $POLICY_ALL_ACCESS, [ref]$policy)
if ($st -ne 0) { throw "LsaOpenPolicy failed, win32 error $([SvLsa]::LsaNtStatusToWinError($st))" }

function Show-Rights {
  $p = [IntPtr]::Zero; $n = 0
  $s = [SvLsa]::LsaEnumerateAccountRights($policy, $sidBytes, [ref]$p, [ref]$n)
  if ($s -ne 0) { Write-Output "rights: (none held; status $([SvLsa]::LsaNtStatusToWinError($s)))"; return }
  $size = [Runtime.InteropServices.Marshal]::SizeOf([Type]([SvLsa+LSA_UNICODE_STRING]))
  $out = @()
  for ($i = 0; $i -lt $n; $i++) {
    $item = [Runtime.InteropServices.Marshal]::PtrToStructure(
      [IntPtr]($p.ToInt64() + $i * $size), [Type]([SvLsa+LSA_UNICODE_STRING]))
    $out += [Runtime.InteropServices.Marshal]::PtrToStringUni($item.Buffer, $item.Length / 2)
  }
  [void][SvLsa]::LsaFreeMemory($p)
  Write-Output ("rights: " + ($out -join ", "))
}

Write-Output "account: $Sid"
Write-Output "before:"; Show-Rights

if (-not $List) {
  $rights = @(New-LsaString $Right)
  if ($Revoke) {
    $st = [SvLsa]::LsaRemoveAccountRights($policy, $sidBytes, $false, $rights, 1)
    $verb = "LsaRemoveAccountRights"
  } else {
    $st = [SvLsa]::LsaAddAccountRights($policy, $sidBytes, $rights, 1)
    $verb = "LsaAddAccountRights"
  }
  if ($st -ne 0) { throw "$verb failed, win32 error $([SvLsa]::LsaNtStatusToWinError($st))" }
  Write-Output "$verb($Right): OK"
  Write-Output "after:"; Show-Rights
}

[void][SvLsa]::LsaClose($policy)
