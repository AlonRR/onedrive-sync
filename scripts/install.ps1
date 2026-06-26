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

# Stop a running tray FIRST — it holds a lock on ods-gui.exe at the destination, so a
# re-install/redeploy can't overwrite it otherwise. Then copy the fresh binaries.
Get-Process ods-gui -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Copy-Item (Join-Path $src 'ods.exe')     $dir -Force
Copy-Item (Join-Path $src 'ods-gui.exe') $dir -Force
$odsExe = Join-Path $dir 'ods.exe'
$guiExe = Join-Path $dir 'ods-gui.exe'
Write-Host "installed -> $dir" -ForegroundColor Green

# Bring the tray back up (no scheduling needed for this).
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

# Register a task, but on a redeploy SKIP the -Force replace when an existing task
# already runs the right exe — replacing a running task needs elevation and would
# fail with "Access is denied", even though nothing needed to change.
function Register-OdsTask($name, $task, $wantExe, $note) {
    $have = Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
    if ($have -and $have.Actions[0].Execute -eq $wantExe) {
        if ($have.State -eq 'Disabled') { Enable-ScheduledTask -TaskName $name | Out-Null }
        Write-Host "$name already current ($note)" -ForegroundColor Green
        return
    }
    try {
        Register-ScheduledTask -TaskName $name -Force -InputObject $task -ErrorAction Stop | Out-Null
        Write-Host "registered $name ($note)" -ForegroundColor Green
    } catch {
        Write-Warning "could not register $name ($($_.Exception.Message.Trim())) — re-run elevated to update it"
    }
}

$a1 = New-ScheduledTaskAction -Execute $odsExe -Argument 'sync'
$tL = New-ScheduledTaskTrigger -AtLogOn
$tR = New-ScheduledTaskTrigger -Once -At (Get-Date) -RepetitionInterval (New-TimeSpan -Minutes 30) -RepetitionDuration (New-TimeSpan -Days 3650)
$s1 = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -StartWhenAvailable -RunOnlyIfNetworkAvailable -ExecutionTimeLimit (New-TimeSpan -Minutes 60)
Register-OdsTask 'ods-sync' (New-ScheduledTask -Action $a1 -Trigger @($tL, $tR) -Settings $s1 -Principal $prin) $odsExe 'logon + every 30 min'

$a2 = New-ScheduledTaskAction -Execute $guiExe
$s2 = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -StartWhenAvailable
Register-OdsTask 'ods-tray' (New-ScheduledTask -Action $a2 -Trigger (New-ScheduledTaskTrigger -AtLogOn) -Settings $s2 -Principal $prin) $guiExe 'logon'

Write-Host ""
Write-Host "ods is LIVE. PowerShell tasks disabled (not deleted). Roll back: scripts\uninstall.ps1" -ForegroundColor Green
