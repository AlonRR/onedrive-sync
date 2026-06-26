<#
.SYNOPSIS
  Install the Rust ods tool and (reversibly) cut the schedule over from the
  PowerShell tool. Safe to re-run. Roll back with uninstall.ps1.

  Binaries + tray launch need no elevation. The schedule swap disables the
  PowerShell tasks FIRST (so the two tools can never sync at once) and aborts
  the swap if it can't — leaving the PowerShell sync live. If that happens,
  re-run this script from an ELEVATED shell to finish the swap.
#>
param(
    # Install prebuilt binaries from a GitHub Release instead of building from source.
    [switch]$FromRelease,
    # A specific release tag (e.g. v0.1.0); implies -FromRelease. Default: the latest release.
    [string]$Version
)
$ErrorActionPreference = 'Stop'

$repo = 'AlonRR/onedrive-sync'
if ($FromRelease -or $Version) {
    $base = if ($Version) { "https://github.com/$repo/releases/download/$Version" }
            else          { "https://github.com/$repo/releases/latest/download" }
    $src = Join-Path $env:TEMP "ods-dl-$PID"
    New-Item -ItemType Directory -Force $src | Out-Null
    foreach ($f in 'ods.exe', 'ods-gui.exe') {
        Write-Host "downloading $f from $base" -ForegroundColor Cyan
        Invoke-WebRequest -Uri "$base/$f" -OutFile (Join-Path $src $f)
    }
} else {
    $src = Resolve-Path (Join-Path $PSScriptRoot '..\target\release')
}
$dir = Join-Path $env:LOCALAPPDATA 'ods'
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item (Join-Path $src 'ods.exe')     $dir -Force
Copy-Item (Join-Path $src 'ods-gui.exe') $dir -Force
$odsExe = Join-Path $dir 'ods.exe'
$guiExe = Join-Path $dir 'ods-gui.exe'
Write-Host "installed -> $dir" -ForegroundColor Green

# Bring up the tray now (no scheduling needed for this).
Get-Process ods-gui -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Process $guiExe
Write-Host "tray launched ($guiExe)" -ForegroundColor Green

# Schedule swap: disable the PowerShell tasks first; abort the swap if we can't.
$canSwap = $true
foreach ($t in 'OneDriveCodeSync', 'OneDriveCodeSyncTray') {
    if (Get-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue) {
        try { Disable-ScheduledTask -TaskName $t -ErrorAction Stop | Out-Null; Write-Host "disabled $t" }
        catch { $canSwap = $false; Write-Warning "cannot disable $t ($($_.Exception.Message.Trim()))" }
    }
}

if (-not $canSwap) {
    Write-Host ""
    Write-Host "Binaries installed and tray running, but the schedule swap needs an ELEVATED shell." -ForegroundColor Yellow
    Write-Host "The PowerShell sync stays LIVE until you re-run this script as administrator." -ForegroundColor Yellow
    return
}

$user = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
$prin = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited

$a1 = New-ScheduledTaskAction -Execute $odsExe -Argument 'sync'
$tL = New-ScheduledTaskTrigger -AtLogOn
$tR = New-ScheduledTaskTrigger -Once -At (Get-Date) -RepetitionInterval (New-TimeSpan -Minutes 30) -RepetitionDuration (New-TimeSpan -Days 3650)
$s1 = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -StartWhenAvailable -RunOnlyIfNetworkAvailable -ExecutionTimeLimit (New-TimeSpan -Minutes 60)
Register-ScheduledTask -TaskName 'ods-sync' -Force -InputObject (New-ScheduledTask -Action $a1 -Trigger @($tL, $tR) -Settings $s1 -Principal $prin) | Out-Null
Write-Host "registered ods-sync (logon + every 30 min)" -ForegroundColor Green

$a2 = New-ScheduledTaskAction -Execute $guiExe
$s2 = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -StartWhenAvailable
Register-ScheduledTask -TaskName 'ods-tray' -Force -InputObject (New-ScheduledTask -Action $a2 -Trigger (New-ScheduledTaskTrigger -AtLogOn) -Settings $s2 -Principal $prin) | Out-Null
Write-Host "registered ods-tray (logon)" -ForegroundColor Green

Write-Host ""
Write-Host "ods is LIVE. PowerShell tasks disabled (not deleted). Roll back: scripts\uninstall.ps1" -ForegroundColor Green
