<#
.SYNOPSIS
    OneDrive 2-way sync — CLI front-end (launcher over onedrive-sync-core.ps1).

.DESCRIPTION
    Thin command surface over the shared core. With no arguments it performs a
    normal (background-style) sync run. All management commands share the core, so
    they behave identically to the tray/GUI.

    On start it applies any pending tool update (per $ToolUpdateMode) so the local
    app copy stays current with the OneDrive source.

.PARAMETER List       Show all known projects and their per-machine status.
.PARAMETER Status     Show a last-run / errors summary.
.PARAMETER SyncNow    Sync one project by id, or all if given '*'/empty.
.PARAMETER Pull       Pull a project local on this machine (skip/undecided -> active).
.PARAMETER Unmap      Stop syncing a project here (keep OneDrive copy). -DeleteLocal also removes the local copy.
.PARAMETER Forget     Retire a project globally (tombstone). OneDrive copy untouched; reversible with -Pull.
.PARAMETER Resync     Force a fresh bisync baseline for a project id (or '*').
.PARAMETER Discover   Interactively choose which available projects to sync locally.
.PARAMETER Conflicts  List unresolved conflict files across active projects.
.PARAMETER Restore    Restore a project (or --File) from the version archive; optional --At <time>.
.PARAMETER Diag       Write a diagnostic bundle (logs + config + state) to %TEMP%. NOT redacted — review before sharing.
.PARAMETER Pause      Disable the scheduled sync task.
.PARAMETER Resume     Re-enable the scheduled sync task.
.PARAMETER Gui        Open the management window (delegates to the tray app).
.PARAMETER DryRun     Preview without changing files.

.EXAMPLE
    .\onedrive-sync.ps1                 # normal sync run
.EXAMPLE
    .\onedrive-sync.ps1 -Discover       # pick which projects to keep local here
.EXAMPLE
    .\onedrive-sync.ps1 -SyncNow my-app
.EXAMPLE
    .\onedrive-sync.ps1 -Restore "Projects\web\my-app" -File src/app.js -At 2026-06-01
#>
[CmdletBinding(DefaultParameterSetName='Run')]
param(
    [switch]$List,
    [switch]$Status,
    [string]$SyncNow,
    [string]$Pull,
    [string]$Unmap,
    [switch]$DeleteLocal,
    [string]$Forget,
    [string]$Resync,
    [switch]$Discover,
    [switch]$Conflicts,
    [string]$Restore,
    [string]$At,
    [string]$File,
    [switch]$Diag,
    [switch]$Pause,
    [switch]$Resume,
    [switch]$Gui,
    [switch]$ApproveDeletes,
    [switch]$DryRun,
    [switch]$NoUpdate,
    [switch]$Help
)

if ($Help) { Get-Help -Full $MyInvocation.MyCommand.Path; return }

. (Join-Path $PSScriptRoot 'onedrive-sync-core.ps1')

$TaskName = 'OneDriveCodeSync'

# Apply any pending tool self-update (no-op in source dir / when -NoUpdate).
if (-not $NoUpdate) {
    try {
        $cfgPre = Import-OdsConfig
        $upd = Update-OdsAppFromSource -Config $cfgPre
        if ($upd -eq 'available') { Write-Host "A tool update is available (set `$ToolUpdateMode='auto' or apply via tray)." -ForegroundColor Cyan }
    } catch { Write-OdsLog "Self-update check failed: $($_.Exception.Message)" 'WARN' }
}

$cfg = Import-OdsConfig

# ---- Interactive console picker (used by -Discover) -------------------------
function Invoke-OdsConsolePicker {
    param([object[]]$Undecided)
    Write-Host ""
    Write-Host "New projects available to sync on this machine:" -ForegroundColor Cyan
    for ($i=0; $i -lt $Undecided.Count; $i++) {
        Write-Host ("  [{0}] {1}  ({2})" -f ($i+1), $Undecided[$i].name, $Undecided[$i].id)
    }
    $ans = Read-Host "Enter numbers to PULL (comma-separated), 'a' for all, Enter to skip all"
    $chosen = @()
    if ($ans -eq 'a') { $chosen = @($Undecided | ForEach-Object { $_.id }) }
    elseif ($ans) {
        foreach ($n in ($ans -split '[,\s]+' | Where-Object { $_ -match '^\d+$' })) {
            $idx = [int]$n - 1
            if ($idx -ge 0 -and $idx -lt $Undecided.Count) { $chosen += $Undecided[$idx].id }
        }
    }
    return $chosen
}

# ---- New watch-root projects: ask for a OneDrive destination ----------------
function Resolve-OdsNewWatchProjects {
    param([hashtable]$Config)
    $known = @(Get-OdsProjects -Config $Config)
    $new = @(Find-OdsNewWatchProjects -Config $Config -Known $known)
    foreach ($w in $new) {
        Write-Host ""
        Write-Host "New local repo with no mapping: $($w.Local)" -ForegroundColor Cyan
        $dest = Read-OdsFolderDialog -Title "Pick OneDrive destination for '$($w.Name)'" -Root (Get-OdsOneDriveRoot)
        if ($dest) {
            $final = Join-Path $dest $w.Name
            try { Add-OdsWatchMapping -Local $w.Local -Dest $final -Config $Config | Out-Null }
            catch { Write-Host "  $($_.Exception.Message)" -ForegroundColor Yellow }
        } else {
            Write-Host "  Skipped (no destination chosen)." -ForegroundColor Yellow
        }
    }
}
function Read-OdsFolderDialog {
    param([string]$Title, [string]$Root)
    try {
        Add-Type -AssemblyName System.Windows.Forms
        $dlg = New-Object System.Windows.Forms.FolderBrowserDialog
        $dlg.Description = $Title
        $dlg.SelectedPath = $Root
        $dlg.ShowNewFolderButton = $true
        if ($dlg.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { return $dlg.SelectedPath }
    } catch {
        $p = Read-Host "$Title (type a full path under OneDrive, or Enter to skip)"
        if ($p) { return $p }
    }
    return $null
}

function Resolve-OdsId {
    <#
      Resolve a (possibly partial) id. An EXACT id always wins. A partial matching
      exactly one project resolves to it. A partial matching several is ambiguous:
      -Destructive throws (never delete/overwrite the wrong project); otherwise the
      first match is returned (legacy convenience).
    #>
    param([string]$Partial, [hashtable]$Config, [switch]$Destructive)
    if (-not $Partial) { return $Partial }
    $all   = @(Get-OdsProjects -Config $Config)
    $exact = @($all | Where-Object { $_.id -eq $Partial })
    if ($exact.Count -ge 1) { return $exact[0].id }
    $hits  = @($all | Where-Object { $_.id -like "*$Partial*" })
    if ($hits.Count -eq 1) { return $hits[0].id }
    if ($hits.Count -gt 1) {
        if ($Destructive) { throw "'$Partial' is ambiguous — matches $($hits.Count) projects: $(($hits | ForEach-Object { $_.id }) -join ', '). Use the exact id." }
        return $hits[0].id
    }
    return $Partial
}

# ============================ Dispatch =======================================
switch ($true) {

    { $List } {
        Get-OdsProjectStatus -Config $cfg |
            Sort-Object Status, Id |
            Format-Table Status, Kind, @{N='Git';E={if($_.Git){'git'}else{'plain'}}}, @{N='Local?';E={if($_.LocalPresent){'yes'}else{'-'}}}, Conflicts, Id -AutoSize
        break
    }

    { $Status } {
        $today = Join-Path $env:LOCALAPPDATA ("onedrive-sync\events\{0}.jsonl" -f (Get-Date).ToUniversalTime().ToString('yyyy-MM-dd'))
        if (Test-Path $today) {
            Write-Host "Recent events (today):" -ForegroundColor Cyan
            Get-Content $today | Select-Object -Last 20 | ForEach-Object { ($_ | ConvertFrom-Json) } |
                Format-Table ts, event, id, code, summary -AutoSize
        } else { Write-Host "No events today." }
        break
    }

    { $Discover } {
        Resolve-OdsNewWatchProjects -Config $cfg
        Invoke-OdsRun -Config $cfg -Interactive -IgnorePause -Decide { param($u) Invoke-OdsConsolePicker -Undecided $u } | Out-Null
        break
    }

    { $PSBoundParameters.ContainsKey('SyncNow') } {
        if (-not $SyncNow -or $SyncNow -eq '*') {
            $cfg2 = $cfg.Clone(); if ($ApproveDeletes) { $cfg2.MaxDeletePercent = 100 }
            Invoke-OdsRun -Config $cfg2 -IgnorePause | Out-Null
        } else {
            $rid  = Resolve-OdsId $SyncNow $cfg
            $proj = @(Get-OdsProjects -Config $cfg) | Where-Object { $_.id -eq $rid } | Select-Object -First 1
            if (-not $proj) { Write-Host "No project matching '$SyncNow'." -ForegroundColor Yellow; break }
            $cfg2 = $cfg.Clone(); if ($ApproveDeletes) { $cfg2.MaxDeletePercent = 100 }
            Sync-OdsProject -Project $proj -Config $cfg2 | Format-List
        }
        break
    }

    { $Pull }    { (Pull-OdsProject  -Id (Resolve-OdsId $Pull $cfg)  -Config $cfg) | Format-List; break }
    { $Unmap }   {  Unmap-OdsProject -Id (Resolve-OdsId $Unmap $cfg -Destructive) -Config $cfg -DeleteLocal:$DeleteLocal; break }
    { $Forget }  {  Forget-OdsProject -Id (Resolve-OdsId $Forget $cfg -Destructive) -Config $cfg; break }

    { $PSBoundParameters.ContainsKey('Resync') } {
        if (-not $Resync -or $Resync -eq '*') {
            foreach ($p in @(Get-OdsProjectStatus -Config $cfg | Where-Object Status -eq 'active')) {
                $proj = @(Get-OdsProjects -Config $cfg) | Where-Object id -eq $p.Id | Select-Object -First 1
                Invoke-OdsBisync -Project $proj -Config $cfg -Resync | Out-Null
            }
        } else {
            $rid  = Resolve-OdsId $Resync $cfg
            $proj = @(Get-OdsProjects -Config $cfg) | Where-Object { $_.id -eq $rid } | Select-Object -First 1
            if ($proj) { Invoke-OdsBisync -Project $proj -Config $cfg -Resync | Out-Null } else { Write-Host "No match." -ForegroundColor Yellow }
        }
        break
    }

    { $Conflicts } {
        $any = $false
        foreach ($p in @(Get-OdsProjects -Config $cfg)) {
            if (-not (Test-Path -LiteralPath $p.local)) { continue }
            $c = @(Get-OdsConflicts -Project $p)
            if ($c.Count) { $any=$true; Write-Host "$($p.id):" -ForegroundColor Yellow; $c | ForEach-Object { "   $_" } }
        }
        if (-not $any) { Write-Host "No unresolved conflicts." -ForegroundColor Green }
        break
    }

    { $Restore } { Restore-OdsItem -Id (Resolve-OdsId $Restore $cfg -Destructive) -Config $cfg -At $At -File $File; break }

    { $Diag } {
        $bundle = Join-Path $env:TEMP ("onedrive-sync-diag-{0}.txt" -f (Get-Date).ToString('yyyyMMdd-HHmmss'))
        "# onedrive-sync diagnostics — NOT redacted; contains full paths, project names and recent log lines. Review before sharing." | Out-File $bundle
        "=== config ==="          | Out-File $bundle -Append
        ($cfg | ConvertTo-Json)   | Out-File $bundle -Append
        "=== machine-state ==="   | Out-File $bundle -Append
        (Get-OdsMachineState | ConvertTo-Json) | Out-File $bundle -Append
        "=== recent log ==="      | Out-File $bundle -Append
        Get-Content (Join-Path $env:LOCALAPPDATA 'onedrive-sync\logs\sync.log') -Tail 200 -ErrorAction SilentlyContinue | Out-File $bundle -Append
        Write-Host "Diagnostic bundle: $bundle" -ForegroundColor Cyan
        break
    }

    { $Pause }  {
        # A flag file is the authoritative pause gate (Invoke-OdsRun skips when present):
        # it never needs elevation, unlike Disable-ScheduledTask which can be access-
        # denied. Also try to disable the task so it does not even spawn, but treat that
        # as a best-effort optimization — the flag is what actually pauses syncing.
        $flag = Join-Path $env:LOCALAPPDATA 'onedrive-sync\paused.flag'
        Set-Content -LiteralPath $flag -Value (Get-Date -Format o) -Encoding ascii
        try { Disable-ScheduledTask -TaskName $TaskName -ErrorAction Stop | Out-Null } catch {}
        Write-Host "Scheduled sync paused (runs skip until -Resume)." -ForegroundColor Yellow
        break
    }
    { $Resume } {
        $flag = Join-Path $env:LOCALAPPDATA 'onedrive-sync\paused.flag'
        Remove-Item -LiteralPath $flag -Force -ErrorAction SilentlyContinue
        try { Enable-ScheduledTask -TaskName $TaskName -ErrorAction Stop | Out-Null } catch {}
        Write-Host "Scheduled sync resumed." -ForegroundColor Green
        break
    }

    { $Gui } {
        $tray = Join-Path $PSScriptRoot 'onedrive-sync-tray.ps1'
        Start-Process pwsh -ArgumentList '-NoProfile','-WindowStyle','Hidden','-File',"`"$tray`"",'-ShowWindow'
        break
    }

    default {
        # Normal run (background semantics: no popups; pending.json + tray surface decisions).
        Invoke-OdsRun -Config $cfg -DryRun:$DryRun | Out-Null
    }
}
