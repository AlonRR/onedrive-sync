<#
.SYNOPSIS
    Stage app-src into the installed app dir and safely restart the tray.

.DESCRIPTION
    The tray and scheduled sync run the INSTALLED copy under
    %LOCALAPPDATA%\onedrive-sync\app — NOT app-src. After editing app-src you must
    stage + restart for the change to take effect (the tray dot-sources core once at
    startup, so it needs a restart to pick up new core; the scheduled CLI re-reads
    core every run).

    SAFETY: the tray-kill match EXCLUDES this process ($PID) and anchors on
    '-File ...onedrive-sync-tray', so it can never match/kill itself. (A naive
    'CommandLine -match "onedrive-sync-tray"' once matched the deploy command's own
    process and Stop-Process killed both trays AND itself.) Verifies exactly one
    healthy tray afterward.

.EXAMPLE
    pwsh -NoProfile -File scripts\deploy.ps1
#>
[CmdletBinding()]
param(
    [string]$SrcDir   = (Join-Path $PSScriptRoot '..\app-src'),
    [string]$AppDir   = (Join-Path $env:LOCALAPPDATA 'onedrive-sync\app'),
    [string]$TrayTask = 'OneDriveCodeSyncTray'
)
$ErrorActionPreference = 'Stop'
$me = $PID

$SrcDir = (Resolve-Path -LiteralPath $SrcDir).Path
if (-not (Test-Path -LiteralPath $AppDir)) {
    throw "Installed app dir not found: $AppDir  (run install-task.ps1 first?)"
}

# --- 1) stage every app-src script (+ VERSION if present) ---
$files = @(Get-ChildItem -LiteralPath $SrcDir -Filter '*.ps1' -File)
foreach ($f in $files) { Copy-Item -LiteralPath $f.FullName -Destination (Join-Path $AppDir $f.Name) -Force }
$ver = Join-Path $SrcDir 'VERSION'
if (Test-Path -LiteralPath $ver) { Copy-Item -LiteralPath $ver -Destination (Join-Path $AppDir 'VERSION') -Force }
Write-Host "staged $($files.Count) script(s) -> $AppDir" -ForegroundColor Green

function Get-OdsTrayProc {
    Get-CimInstance Win32_Process -Filter "Name='powershell.exe' OR Name='pwsh.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.ProcessId -ne $me -and $_.CommandLine -match '-File.+onedrive-sync-tray' }
}

# --- 2) stop the running tray (NEVER self: $PID excluded + -File anchored) ---
foreach ($p in @(Get-OdsTrayProc)) {
    Write-Host "stopping tray PID $($p.ProcessId)" -ForegroundColor Yellow
    Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue
    try { Wait-Process -Id $p.ProcessId -Timeout 5 -ErrorAction SilentlyContinue } catch {}
}

# --- 3) restart + verify a single, stable instance ---
Start-ScheduledTask -TaskName $TrayTask
Start-Sleep -Seconds 3
$now = @(Get-OdsTrayProc)
if ($now.Count -eq 0) {
    throw "Tray did not start from task '$TrayTask'. Check: Get-ScheduledTaskInfo -TaskName $TrayTask"
}
if ($now.Count -gt 1) {
    Write-Warning "Multiple tray instances running: $(($now.ProcessId) -join ', ') — expected exactly one."
    exit 1
}
Start-Sleep -Seconds 2   # a load-time error would crash it within ~1s
if (-not (Get-Process -Id $now[0].ProcessId -ErrorAction SilentlyContinue)) {
    throw "Tray PID $($now[0].ProcessId) exited right after start — likely a load error in the deployed code."
}
Write-Host "tray healthy: PID $($now[0].ProcessId) [$($now[0].Name)]" -ForegroundColor Green
