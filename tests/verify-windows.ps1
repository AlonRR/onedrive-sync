<#
.SYNOPSIS
    Headless check that every tray window actually BUILDS and opens without error.

.DESCRIPTION
    verify-handlers.ps1 proves handlers resolve statically; this proves the other
    half — that each Show-* window's build code (New-WpfWindow, FindName, the body
    before ShowDialog) runs without a null-ref / bad-FindName that would crash that
    GUI option. Each window is opened for real and closed immediately from a
    DispatcherTimer (build + close modifies nothing). STA/5.1-only; sibling of
    run-tests.ps1.

.EXAMPLE
    powershell.exe -NoProfile -STA -ExecutionPolicy Bypass -File tests\verify-windows.ps1
#>
param(
    [string]$TrayPath = (Join-Path (Split-Path $PSScriptRoot -Parent) 'app-src\onedrive-sync-tray.ps1'),
    [string]$SampleProjectId
)
$ErrorActionPreference = 'Stop'

. $TrayPath -NoStart

# Stand-ins for tray-only globals the management window's refresh touches.
function Update-TrayIcon {}
$script:timer = New-Object System.Windows.Forms.Timer

# Pick a real git project id for Show-OdsProjectSettings if none supplied.
if (-not $SampleProjectId) {
    $p = @(Get-OdsProjects -Config (Get-Cfg)) | Select-Object -First 1
    if ($p) { $SampleProjectId = $p.id }
}

function Test-Window {
    param([string]$Label, [scriptblock]$Open)
    $script:winErr = $null
    $dt = New-Object System.Windows.Threading.DispatcherTimer
    $dt.Interval = [TimeSpan]::FromMilliseconds(700)
    $dt.Add_Tick({
        $script:WinForceClose = $true   # Show-OdsWindow cancels normal close; force it
        $app = [System.Windows.Application]::Current
        if ($app) { foreach ($w in @($app.Windows)) { try { $w.Close() } catch {} } }
        $dt.Stop()
    })
    $dt.Start()
    try { & $Open | Out-Null }
    catch { $script:winErr = "$($_.Exception.GetType().Name): $($_.Exception.Message.Split([char]10)[0])" }
    finally { $dt.Stop() }
    if ($script:winErr) { Write-Host "  FAIL  $Label : $script:winErr" -ForegroundColor Red; return $false }
    Write-Host "  OK    $Label" -ForegroundColor Green; return $true
}

$results = @()
$results += Test-Window 'Show-OdsWindow (Manage)'        { Show-OdsWindow }
$results += Test-Window 'Show-OdsSettings'               { Show-OdsSettings }
$results += Test-Window 'Show-OdsWatch'                  { Show-OdsWatch }
$results += Test-Window 'Show-OdsPicker'                 { Show-OdsPicker }
$results += Test-Window 'Show-OdsRetired'                { Show-OdsRetired }
$results += Test-Window "Show-OdsProjectSettings ($SampleProjectId)" { Show-OdsProjectSettings -ProjectId $SampleProjectId }

$failed = @($results | Where-Object { -not $_ }).Count
Write-Host ""
if ($failed) { Write-Host "$failed window(s) failed to open." -ForegroundColor Red; exit 1 }
Write-Host "All windows opened and closed cleanly." -ForegroundColor Green
exit 0
