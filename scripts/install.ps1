<#
.SYNOPSIS
  Install the Rust ods tool and (reversibly) cut the schedule over from the
  PowerShell tool. Safe to re-run. Roll back with uninstall.ps1.

  Binaries + tray launch need no elevation. The schedule swap disables the
  PowerShell tasks FIRST (so the two tools can never sync at once) and aborts
  the swap if it can't -- leaving the PowerShell sync live. If that happens,
  re-run this script from an ELEVATED shell to finish the swap.

  Adds %LOCALAPPDATA%\ods to the per-user PATH so `ods <cmd>` works from any
  terminal, and preflights the rclone + git runtime deps (winget auto-install
  unless -SkipDeps). Downloaded release binaries are checksum-verified.
#>
param(
    # Install prebuilt binaries from a GitHub Release instead of building from source.
    [switch]$FromRelease,
    # A specific release tag (e.g. v0.1.0); implies -FromRelease. Default: the latest release.
    [string]$Version,
    # Skip the rclone/git preflight (don't check for or auto-install the runtime deps).
    [switch]$SkipDeps
)
$ErrorActionPreference = 'Stop'

$repo = 'AlonRR/onedrive-sync'

# --- Preflight: rclone + git are needed at RUN time (ods shells out to both), not
# to install. A miss only warns (and offers a winget install) and never aborts the
# binary/task install, so the tool still lands and the dep can be added later.
function Install-OdsDep($exe, $wingetId, $label) {
    if (Get-Command $exe -ErrorAction SilentlyContinue) {
        Write-Host "found $exe" -ForegroundColor Green
        return
    }
    Write-Warning "$label ('$exe') is not on PATH -- ods sync will fail until it is installed."
    if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
        Write-Host "  winget not found -- install $label manually (rclone.org/downloads or git-scm.com)." -ForegroundColor Yellow
        return
    }
    Write-Host "installing $label via winget ($wingetId)..." -ForegroundColor Cyan
    try {
        winget install --id $wingetId -e --source winget --accept-package-agreements --accept-source-agreements
    } catch {
        Write-Warning "winget install of $wingetId failed ($($_.Exception.Message.Trim())); install $label manually."
        return
    }
    if (Get-Command $exe -ErrorAction SilentlyContinue) {
        Write-Host "installed $exe" -ForegroundColor Green
    } else {
        Write-Warning "winget ran but '$exe' still isn't on PATH -- open a new terminal, or install $label manually."
    }
}
if ($SkipDeps) {
    Write-Host "skipping rclone/git preflight (-SkipDeps)" -ForegroundColor Yellow
} else {
    Install-OdsDep 'git'    'Git.Git'       'git'
    Install-OdsDep 'rclone' 'Rclone.Rclone' 'rclone'
}

if ($FromRelease -or $Version) {
    $base = if ($Version) { "https://github.com/$repo/releases/download/$Version" }
            else          { "https://github.com/$repo/releases/latest/download" }
    $src = Join-Path $env:TEMP "ods-dl-$PID"
    New-Item -ItemType Directory -Force $src | Out-Null
    foreach ($f in 'ods.exe', 'ods-gui.exe') {
        Write-Host "downloading $f from $base" -ForegroundColor Cyan
        $out = Join-Path $src $f
        Invoke-WebRequest -Uri "$base/$f" -OutFile $out
        # Verify against the published SHA256 sidecar. Older releases may not have
        # one -- treat a missing checksum as skip-with-warning, a mismatch as fatal
        # (never run an exe that doesn't match its release hash).
        $want = $null
        try {
            $want = (Invoke-WebRequest -Uri "$base/$f.sha256" -UseBasicParsing).Content
        } catch {
            Write-Warning "no published checksum for $f -- skipping verification."
        }
        if ($want) {
            $want = ($want -replace '[^0-9A-Fa-f]', '')
            $have = (Get-FileHash $out -Algorithm SHA256).Hash
            if ($want -and $have -ieq $want) {
                Write-Host "verified $f (sha256 ok)" -ForegroundColor Green
            } else {
                throw "checksum mismatch for $f (expected $want, got $have) -- aborting."
            }
        }
    }
} else {
    $src = Resolve-Path (Join-Path $PSScriptRoot '..\target\release')
}
$dir = Join-Path $env:LOCALAPPDATA 'ods'
New-Item -ItemType Directory -Force $dir | Out-Null

# Stop a running tray FIRST -- it holds a lock on ods-gui.exe at the destination, so a
# re-install/redeploy can't overwrite it otherwise. Then copy the fresh binaries.
Get-Process ods-gui -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Copy-Item (Join-Path $src 'ods.exe')     $dir -Force
Copy-Item (Join-Path $src 'ods-gui.exe') $dir -Force
$odsExe = Join-Path $dir 'ods.exe'
$guiExe = Join-Path $dir 'ods-gui.exe'
Write-Host "installed -> $dir" -ForegroundColor Green

# Put ods on the per-user PATH so `ods <cmd>` works from any terminal (the Start
# Menu shortcut only covers the GUI). Read the RAW (unexpanded) user Path from the
# registry and write it back as REG_EXPAND_SZ -- NOT [Environment]::*EnvironmentVariable
# with 'User', which returns the already-expanded value and, re-saved, both clobbers
# other apps' %VAR% entries and can hit the legacy 1024-char setx truncation.
$userPath = (Get-ItemProperty -Path 'HKCU:\Environment' -Name Path -ErrorAction SilentlyContinue).Path
$parts = @($userPath -split ';' | Where-Object { $_ -ne '' })
if ($parts -notcontains $dir) {
    $newPath = ($parts + $dir) -join ';'
    Set-ItemProperty -Path 'HKCU:\Environment' -Name Path -Value $newPath -Type ExpandString
    # Broadcast WM_SETTINGCHANGE so newly-opened terminals see the new PATH without a
    # logoff (new processes inherit from explorer.exe, which only refreshes on this).
    try {
        if (-not ('Win32.OdsEnv' -as [type])) {
            Add-Type -Namespace Win32 -Name OdsEnv -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll", SetLastError=true, CharSet=System.Runtime.InteropServices.CharSet.Auto)]
public static extern System.IntPtr SendMessageTimeout(System.IntPtr hWnd, uint Msg, System.IntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out System.IntPtr lpdwResult);
'@
        }
        $res = [System.IntPtr]::Zero
        [void][Win32.OdsEnv]::SendMessageTimeout([System.IntPtr]0xffff, 0x1a, [System.IntPtr]::Zero, 'Environment', 2, 5000, [ref]$res)
    } catch { }
    $env:Path = "$env:Path;$dir"   # usable in THIS session too
    Write-Host "added $dir to PATH (open a NEW terminal to use 'ods')" -ForegroundColor Green
} else {
    Write-Host "PATH already contains $dir" -ForegroundColor Green
}

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
# already runs the right exe -- replacing a running task needs elevation and would
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
        Write-Warning "could not register $name ($($_.Exception.Message.Trim())) - re-run elevated to update it"
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

# Start Menu shortcut (per-user, no elevation) + Settings > Apps entry (HKCU, no
# elevation). Both point at the exes in $dir, so no separate .ico or copied script
# is needed -- DisplayIcon/IconLocation reference ods-gui.exe's own embedded icon,
# and UninstallString runs the native `ods uninstall` subcommand directly.
$startMenu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
$shortcut = Join-Path $startMenu 'ods (OneDrive Sync).lnk'
$wsh = New-Object -ComObject WScript.Shell
$sc = $wsh.CreateShortcut($shortcut)
$sc.TargetPath = $guiExe
$sc.WorkingDirectory = $dir
$sc.IconLocation = "$guiExe,0"
$sc.Description = 'ods - OneDrive two-way code sync'
$sc.Save()
Write-Host "Start Menu shortcut -> $shortcut" -ForegroundColor Green

$verOut = & $odsExe --version 2>$null
$version = if ($verOut -match '(\d+\.\d+\.\d+)') { $matches[1] } else { '0.0.0' }
$sizeKb = [math]::Round(((Get-Item $odsExe).Length + (Get-Item $guiExe).Length) / 1KB)
$uninstKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\ods'
New-Item -Path $uninstKey -Force | Out-Null
Set-ItemProperty -Path $uninstKey -Name DisplayName -Value 'ods (OneDrive Sync)'
Set-ItemProperty -Path $uninstKey -Name DisplayVersion -Value $version
Set-ItemProperty -Path $uninstKey -Name Publisher -Value 'Alon Rabinowitz'
Set-ItemProperty -Path $uninstKey -Name InstallLocation -Value $dir
Set-ItemProperty -Path $uninstKey -Name DisplayIcon -Value "$guiExe,0"
Set-ItemProperty -Path $uninstKey -Name UninstallString -Value "`"$odsExe`" uninstall"
New-ItemProperty -Path $uninstKey -Name EstimatedSize -Value $sizeKb -PropertyType DWord -Force | Out-Null
New-ItemProperty -Path $uninstKey -Name NoModify -Value 1 -PropertyType DWord -Force | Out-Null
New-ItemProperty -Path $uninstKey -Name NoRepair -Value 1 -PropertyType DWord -Force | Out-Null
Write-Host "Installed Apps entry registered (v$version)" -ForegroundColor Green

Write-Host ""
Write-Host "ods is LIVE. PowerShell tasks disabled (not deleted). Roll back: scripts\uninstall.ps1" -ForegroundColor Green
Write-Host "Full removal: 'ods uninstall', or Settings > Apps > Installed apps > ods." -ForegroundColor Green
