<#
.SYNOPSIS
    Installs (or removes) the OneDrive 2-way sync tool on this machine.

.DESCRIPTION
    Run once per machine from the OneDrive source folder. It:
      1. Ensures git + rclone are present (auto-installs if missing).
      2. Migrates away from the old one-way OneDriveCodeSync tool.
      3. Stages the scripts from app-src\ to %LOCALAPPDATA%\onedrive-sync\app\.
      4. Registers the scheduled sync task and the tray helper (both at logon),
         pointing at the LOCAL app copies (never running code from OneDrive).

.PARAMETER IntervalMinutes  How often to sync (default 30).
.PARAMETER Uninstall        Remove the tasks, tray, and (optionally) local state.

.EXAMPLE
    .\install-task.ps1
.EXAMPLE
    .\install-task.ps1 -IntervalMinutes 15
.EXAMPLE
    .\install-task.ps1 -Uninstall
#>
param(
    [int]$IntervalMinutes = 30,
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'
$TaskName     = 'OneDriveCodeSync'
$TrayTaskName = 'OneDriveCodeSyncTray'
$LocalRoot    = Join-Path $env:LOCALAPPDATA 'onedrive-sync'
$AppDir       = Join-Path $LocalRoot 'app'
$SrcDir       = Join-Path $PSScriptRoot 'app-src'
$RclonePin    = 'v1.69.1'   # >= 1.66 for bisync --backup-dir1
# Prefer PS7 (pwsh) for the sync task; tray uses powershell.exe for STA/WinForms.
$PwshExe      = if (Get-Command pwsh -ErrorAction SilentlyContinue) { 'pwsh.exe' } else { 'powershell.exe' }
$PwshTrayExe  = 'powershell.exe'   # tray relaunches itself under WPS -STA for WinForms

function Write-Step($m) { Write-Host "==> $m" -ForegroundColor Cyan }

# ---------------------------------------------------------------- Uninstall
if ($Uninstall) {
    foreach ($t in $TaskName, $TrayTaskName) {
        if (Get-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue) {
            Unregister-ScheduledTask -TaskName $t -Confirm:$false
            Write-Host "Removed task '$t'." -ForegroundColor Green
        }
    }
    Get-CimInstance Win32_Process -Filter "Name='powershell.exe' OR Name='pwsh.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -match 'onedrive-sync-tray' } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    $bisync = Join-Path $LocalRoot 'bisync'
    if (Test-Path $bisync) { Remove-Item $bisync -Recurse -Force -ErrorAction SilentlyContinue }
    $ans = Read-Host "Also delete local version archive + state at $LocalRoot ? (y/N)"
    if ($ans -eq 'y') { Remove-Item $LocalRoot -Recurse -Force -ErrorAction SilentlyContinue; Write-Host "Removed $LocalRoot." }
    else { Write-Host "Kept $LocalRoot (versions/state preserved)." }
    return
}

# ---------------------------------------------------------------- Prereqs
if (-not $env:OneDriveConsumer) {
    throw "`$env:OneDriveConsumer is not set — start/sign-in the personal OneDrive client first."
}
New-Item -ItemType Directory -Force -Path $LocalRoot, $AppDir | Out-Null

function Install-OdsGit {
    if (Get-Command git -ErrorAction SilentlyContinue) { Write-Host "git: present" -ForegroundColor Green; return }
    Write-Step "Installing git (winget)…"
    winget install --id Git.Git -e --source winget --accept-source-agreements --accept-package-agreements
    $env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) { throw "git still not found after install. Install Git and re-run." }
}

function Install-OdsRclone {
    $dest = Join-Path $LocalRoot 'rclone.exe'
    if (Test-Path $dest) { Write-Host "rclone: present ($dest)" -ForegroundColor Green; return }
    Write-Step "Downloading rclone $RclonePin…"
    $arch = if ([Environment]::Is64BitOperatingSystem) { 'windows-amd64' } else { 'windows-386' }
    $base = "https://downloads.rclone.org/$RclonePin"
    $zipName = "rclone-$RclonePin-$arch.zip"
    $tmp = Join-Path $env:TEMP $zipName
    $sums = Join-Path $env:TEMP "rclone-$RclonePin-SHA256SUMS"
    Invoke-WebRequest "$base/$zipName" -OutFile $tmp
    Invoke-WebRequest "$base/SHA256SUMS" -OutFile $sums
    $want = (Select-String -Path $sums -Pattern ([regex]::Escape($zipName)) | Select-Object -First 1).Line.Split(' ')[0]
    $got  = (Get-FileHash $tmp -Algorithm SHA256).Hash
    if ($want -and $got -ne $want.ToUpper()) { throw "rclone checksum mismatch (got $got, want $want)." }
    Write-Host "  checksum OK" -ForegroundColor Green
    $ex = Join-Path $env:TEMP "rclone-extract-$([guid]::NewGuid().ToString('N').Substring(0,6))"
    Expand-Archive -Path $tmp -DestinationPath $ex -Force
    Copy-Item (Get-ChildItem $ex -Recurse -Filter rclone.exe | Select-Object -First 1).FullName $dest -Force
    Remove-Item $tmp, $sums, $ex -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "rclone installed -> $dest" -ForegroundColor Green
}

# ---------------------------------------------------------------- Migration (E81)
function Update-OdsFromOldTool {
    $old = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if ($old) {
        $action = ($old.Actions | Select-Object -First 1).Arguments
        if ($action -and $action -notmatch 'app\\onedrive-sync.ps1') {
            Write-Step "Removing the old one-way OneDriveCodeSync task…"
            Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        }
    }
    $oldCfg = Join-Path $PSScriptRoot 'sync-config.ps1'
    if (Test-Path $oldCfg) {
        $oldHash = (Get-FileHash $oldCfg).Hash
        $newHash = (Get-FileHash (Join-Path $SrcDir 'sync-config.ps1')).Hash
        if ($oldHash -ne $newHash) {
            Write-Host "Note: an old sync-config.ps1 exists at the tool root. Review app-src\sync-config.ps1 and port any custom roots." -ForegroundColor Yellow
        }
    }
}

# ---------------------------------------------------------------- Staging
function Copy-OdsApp {
    Write-Step "Staging app to $AppDir…"
    Get-ChildItem $SrcDir -File | ForEach-Object { Copy-Item $_.FullName (Join-Path $AppDir $_.Name) -Force }
    Write-Host "Staged $((Get-ChildItem $AppDir -File).Count) files." -ForegroundColor Green
    try { Set-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem' -Name LongPathsEnabled -Value 1 -ErrorAction Stop }
    catch { Write-Host "  (could not enable LongPathsEnabled — run as admin if you hit >260-char paths)" -ForegroundColor DarkYellow }
}

# ---------------------------------------------------------------- Scheduling
function Register-OdsTasks {
    $syncScript = Join-Path $AppDir 'onedrive-sync.ps1'
    $trayScript = Join-Path $AppDir 'onedrive-sync-tray.ps1'
    $user = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name

    $a1 = New-ScheduledTaskAction -Execute $PwshExe -Argument "-NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$syncScript`""
    $tLogon  = New-ScheduledTaskTrigger -AtLogOn
    # Finite (10-year) duration — [timespan]::MaxValue is rejected/clamped by Task
    # Scheduler on some Windows builds, making the repetition silently never arm.
    $tRepeat = New-ScheduledTaskTrigger -Once -At (Get-Date) -RepetitionInterval (New-TimeSpan -Minutes $IntervalMinutes) -RepetitionDuration (New-TimeSpan -Days 3650)
    $set = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -StartWhenAvailable -RunOnlyIfNetworkAvailable -ExecutionTimeLimit (New-TimeSpan -Minutes 60)
    $prin = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
    $task = New-ScheduledTask -Action $a1 -Trigger @($tLogon, $tRepeat) -Settings $set -Principal $prin -Description "OneDrive 2-way sync (every $IntervalMinutes min)."
    Register-ScheduledTask -TaskName $TaskName -InputObject $task -Force | Out-Null
    Write-Host "Registered '$TaskName' (logon + every $IntervalMinutes min)." -ForegroundColor Green

    $a2 = New-ScheduledTaskAction -Execute $PwshTrayExe -Argument "-NoProfile -STA -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$trayScript`""
    $set2 = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -StartWhenAvailable
    $task2 = New-ScheduledTask -Action $a2 -Trigger (New-ScheduledTaskTrigger -AtLogOn) -Settings $set2 -Principal $prin -Description "OneDrive sync tray helper."
    Register-ScheduledTask -TaskName $TrayTaskName -InputObject $task2 -Force | Out-Null
    Write-Host "Registered '$TrayTaskName' (logon)." -ForegroundColor Green
}

# ---------------------------------------------------------------- Run
Install-OdsGit
Install-OdsRclone
Update-OdsFromOldTool
Copy-OdsApp
Register-OdsTasks

Write-Host ""
Write-Host "Installed." -ForegroundColor Green
Write-Host ""
Write-Host "ACTION REQUIRED: In File Explorer, right-click each OneDrive project parent folder" -ForegroundColor Yellow
Write-Host "  and choose 'Always keep on this device' so the files are locally present for sync." -ForegroundColor Yellow
Write-Host ""
$ans = Read-Host "Have you done that (or want to skip for now)? [Y/n]"
if ($ans -ne 'n' -and $ans -ne 'N') {
    Write-Host ""
    Write-Host "Running -Discover..." -ForegroundColor Cyan
    $syncScript = Join-Path $AppDir 'onedrive-sync.ps1'
    # -STA required for FolderBrowserDialog; always use powershell.exe here (pwsh doesn't support -STA).
    & $PwshTrayExe -NoProfile -STA -ExecutionPolicy Bypass -File $syncScript -Discover
}

Write-Host ""
Write-Host "Starting tray helper..." -ForegroundColor Cyan
$trayScript = Join-Path $AppDir 'onedrive-sync-tray.ps1'
Start-Process $PwshTrayExe -ArgumentList @('-NoProfile', '-WindowStyle', 'Hidden', '-ExecutionPolicy', 'Bypass', '-File', "`"$trayScript`"")
Write-Host "Tray launched (check your system tray)." -ForegroundColor Green
