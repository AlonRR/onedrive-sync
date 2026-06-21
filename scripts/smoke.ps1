<#
.SYNOPSIS
    Read-only health check: dry-run a sync, then report the run summary, recent
    errors, and stale lock/temp debris. Changes no synced files.

.DESCRIPTION
    Runs the installed CLI with -DryRun -NoUpdate (rclone --dry-run => read-only;
    -DryRun also skips the version-prune). Then summarizes today's event log and
    the recent sync.log so you can confirm "did I break syncing?" in ~30s.

    NOTE: an ERROR line is not always a problem — rclone's max-delete safety abort
    ("Safety abort: too many deletes") is the guard WORKING and shows up as ERROR.

.EXAMPLE
    pwsh -NoProfile -File scripts\smoke.ps1
#>
[CmdletBinding()]
param(
    [string]$AppDir  = (Join-Path $env:LOCALAPPDATA 'onedrive-sync\app'),
    [string]$DataDir = (Join-Path $env:LOCALAPPDATA 'onedrive-sync')
)
$ErrorActionPreference = 'Stop'
$cli = Join-Path $AppDir 'onedrive-sync.ps1'
if (-not (Test-Path -LiteralPath $cli)) { throw "CLI not found: $cli (run scripts\deploy.ps1 first?)" }
$pwshExe = (Get-Command pwsh -ErrorAction SilentlyContinue).Source
if (-not $pwshExe) { $pwshExe = (Get-Command powershell).Source }

Write-Host "running read-only dry-run (no files changed)..." -ForegroundColor Cyan
& $pwshExe -NoProfile -ExecutionPolicy Bypass -File $cli -DryRun -NoUpdate *> $null
Write-Host "dry-run exit code: $LASTEXITCODE"

# --- today's events: run summary + bisync count ---
$ev = Join-Path $DataDir ("events\{0}.jsonl" -f [datetime]::UtcNow.ToString('yyyy-MM-dd'))
if (Test-Path -LiteralPath $ev) {
    $events = @(Get-Content -LiteralPath $ev -Tail 200 | ForEach-Object { try { $_ | ConvertFrom-Json } catch {} } | Where-Object { $_ })
    $end = @($events | Where-Object { $_.event -eq 'run-end' })
    if ($end.Count) { Write-Host "last run-end: $($end[-1].summary)" -ForegroundColor Green }
    Write-Host "bisync events today: $(@($events | Where-Object { $_.event -eq 'bisync' }).Count)"
} else { Write-Host "no events today." }

# --- recent ERROR lines (informational; some are rclone safety guards) ---
$log = Join-Path $DataDir 'logs\sync.log'
if (Test-Path -LiteralPath $log) {
    $errs = @(Get-Content -LiteralPath $log -Tail 200 | Select-String -SimpleMatch '[ERROR]')
    if ($errs.Count) {
        Write-Host "recent ERROR lines ($($errs.Count)) — review (safety-aborts are OK):" -ForegroundColor Yellow
        $errs | Select-Object -Last 5 | ForEach-Object { "  $($_.Line)" }
    } else { Write-Host "no recent ERROR lines." -ForegroundColor Green }
}

# --- stale debris (locks should be released; temps should be swept) ---
$locks = @(Get-ChildItem -LiteralPath $DataDir -Recurse -Include '*.lock', '.lock' -File -ErrorAction SilentlyContinue)
$tmps  = @(Get-ChildItem -LiteralPath $DataDir -Filter '*.tmp.*' -File -ErrorAction SilentlyContinue)
$colour = if (($locks.Count + $tmps.Count) -gt 0) { 'Yellow' } else { 'Green' }
Write-Host ("stale locks: {0}   stale temps: {1}" -f $locks.Count, $tmps.Count) -ForegroundColor $colour
