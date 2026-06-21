# OneDrive sync core — run orchestration & public mutations
# (dot-sourced by onedrive-sync-core.ps1).

# ----------------------------------------------------------------------------
#region  Per-project sync
# ----------------------------------------------------------------------------
function Sync-OdsProject {
    param([object]$Project, [hashtable]$Config, [switch]$DryRun, [switch]$Force, [switch]$SkipGate)

    if (-not $SkipGate -and -not $DryRun) {
        $gate = Test-OdsGate -Project $Project -Config $Config
        if (-not $gate.Ok) {
            return [pscustomobject]@{ Id=$Project.id; Status='deferred'; Reason=$gate.Reason; Transient=$gate.Transient }
        }
    }

    $baseline = Test-OdsBaselineExists -Project $Project
    if (-not $baseline -and -not $DryRun) { Invoke-OdsSeed -Project $Project -Config $Config }

    $resync = (-not $baseline)
    $code = Invoke-OdsBisync -Project $Project -Config $Config -Resync:$resync -DryRun:$DryRun -Force:$Force
    if ($resync -and $code -ge 8) {
        # resync failed; surface
        return [pscustomobject]@{ Id=$Project.id; Status='error'; Reason="resync exit $code" }
    }
    # rclone bisync: 0 success, 1-7 partial/warnings, >=8 fatal
    $status = if ($code -eq 0) { 'ok' } elseif ($code -lt 8) { 'warn' } else { 'error' }

    if (-not $DryRun -and $Project.git) {
        Resolve-OdsDivergence -Project $Project
        if (-not (Test-OdsGitIntegrity -Project $Project)) {
            # don't keep a corrupt baseline -> wipe workdir listing so next run resyncs
            Reset-OdsBaseline -Id $Project.id -ListingOnly
            $status = 'error'
        }
    }

    $conflicts = @(Get-OdsConflicts -Project $Project)
    if ($conflicts.Count -gt 0) {
        Write-OdsLog "$($Project.id): $($conflicts.Count) conflict file(s) need attention." 'WARN'
        $status = 'conflict'
    }
    [pscustomobject]@{ Id=$Project.id; Status=$status; Code=$code; Conflicts=$conflicts.Count }
}
#endregion

# ----------------------------------------------------------------------------
#region  Reconcile pass (F)
# ----------------------------------------------------------------------------
function Invoke-OdsReconcile {
    param([hashtable]$Config, [object[]]$Projects)
    $validIds = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($p in $Projects) { [void]$validIds.Add($p.id) }

    # prune machine-state entries for ids that no longer exist (E64)
    Edit-OdsMachineState -Mutate {
        param($state)
        $state.active = @($state.active | Where-Object { $validIds.Contains($_) })
        $state.skip   = @($state.skip   | Where-Object { $validIds.Contains($_) })
    }
    $state = Get-OdsMachineState

    # vanished side detection (E48/E68/E69): handled lazily in the run loop, but
    # warn here for visibility.
    foreach ($p in $Projects) {
        if ($state.active -contains $p.id) {
            $localGone = -not (Test-Path -LiteralPath $p.local)
            $destGone  = -not (Test-Path -LiteralPath $p.dest)
            if ($localGone -xor $destGone) {
                Write-OdsLog "Reconcile: $($p.id) has one side missing (local=$(!$localGone) dest=$(!$destGone)); will skip + keep present side." 'WARN'
            }
        }
    }
}
#endregion

# ----------------------------------------------------------------------------
#region  Main run (§1b)
# ----------------------------------------------------------------------------
function Invoke-OdsRun {
    <#
      The scheduled/CLI run. -Decide is an optional scriptblock that receives the
      list of undecided projects and returns the ids to activate (interactive).
      With no -Decide, undecided projects are written to pending.json (background).
    #>
    param([hashtable]$Config, [scriptblock]$Decide, [switch]$DryRun, [switch]$Interactive)

    if (-not (Enter-OdsLock)) { return }
    try {
        Write-OdsEvent 'run-start' @{ dryrun=[bool]$DryRun; interactive=[bool]$Interactive }
        $projects = @(Get-OdsProjects -Config $Config)
        Invoke-OdsReconcile -Config $Config -Projects $projects

        $state = Get-OdsMachineState
        $activeSet = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($a in $state.active) { [void]$activeSet.Add($a) }
        $skipSet = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($s in $state.skip) { [void]$skipSet.Add($s) }

        # Classify undecided.
        $undecided = [System.Collections.Generic.List[object]]::new()
        foreach ($p in $projects) {
            if ($activeSet.Contains($p.id) -or $skipSet.Contains($p.id)) { continue }
            $localPresent = Test-Path -LiteralPath $p.local
            if ($localPresent -and $p.kind -eq 'mirror') {
                # new local project -> auto-activate + push (§1b.3)
                Set-OdsState -Id $p.id -Status active
                [void]$activeSet.Add($p.id)
                Write-OdsLog "Auto-activated new local project $($p.id)." 'INFO'
            } else {
                $undecided.Add($p)
            }
        }

        if ($undecided.Count -gt 0) {
            if ($Decide) {
                $chosen = & $Decide $undecided
                foreach ($p in $undecided) {
                    if ($chosen -contains $p.id) { Set-OdsState -Id $p.id -Status active; [void]$activeSet.Add($p.id) }
                    else { Set-OdsState -Id $p.id -Status skip }
                }
            } else {
                Write-OdsPending -Undecided $undecided
                Write-OdsLog "$($undecided.Count) new project(s) available; awaiting decision (tray/-Discover)." 'INFO'
            }
        } else {
            Write-OdsPending -Undecided @()
        }

        # Active projects, prioritized by most-recent change, time-budgeted.
        $active = @($projects | Where-Object { $activeSet.Contains($_.id) } | Sort-Object -Property @{Expression={ Get-OdsLastChange $_ }} -Descending)
        $deadline = (Get-Date).AddSeconds($Config.RunTimeBudget)
        $deferred = [System.Collections.Generic.List[object]]::new()
        $results  = [System.Collections.Generic.List[object]]::new()

        foreach ($p in $active) {
            if ((Get-Date) -gt $deadline) {
                Write-OdsLog "Time budget reached; carrying $($p.id) to next cycle." 'INFO'
                $deferred.Add($p); continue
            }
            # vanished-side guard
            $localGone = -not (Test-Path -LiteralPath $p.local)
            $destGone  = -not (Test-Path -LiteralPath $p.dest)
            if ($localGone -or $destGone) {
                Write-OdsLog "$($p.id): a side is missing (keeping present side, not propagating deletion)." 'WARN'
                continue
            }
            $r = Sync-OdsProject -Project $p -Config $Config -DryRun:$DryRun
            $results.Add($r)
            if ($r.Status -eq 'deferred' -and $r.Transient) { $deferred.Add($p) }
        }

        # Smart-retry of transiently-gated repos with backoff.
        if ($deferred.Count -gt 0 -and -not $DryRun) {
            $waited = 0; $attempt = 0
            while ($deferred.Count -gt 0 -and $attempt -lt $Config.RetryMaxAttempts -and $waited -lt $Config.RetryMaxWaitSeconds -and (Get-Date) -lt $deadline) {
                $sleep = $Config.RetryBackoff[[math]::Min($attempt, $Config.RetryBackoff.Count-1)]
                Start-Sleep -Seconds $sleep; $waited += $sleep; $attempt++
                $still = [System.Collections.Generic.List[object]]::new()
                foreach ($p in $deferred) {
                    $r = Sync-OdsProject -Project $p -Config $Config
                    if ($r.Status -eq 'deferred') { $still.Add($p) } else { $results.Add($r) }
                }
                $deferred = $still
            }
            foreach ($p in $deferred) {
                Update-OdsDeferCount -Id $p.id -Config $Config
                Write-OdsLog "Deferring $($p.id) to next cycle." 'INFO'
            }
        }

        Invoke-OdsVersionPrune -Config $Config
        $summary = ($results | Group-Object Status | ForEach-Object { "$($_.Name)=$($_.Count)" }) -join ' '
        Write-OdsLog "Run complete. $summary" 'INFO'
        Write-OdsEvent 'run-end' @{ summary=$summary; deferred=$deferred.Count }
        return $results
    } finally {
        Exit-OdsLock
    }
}

function Get-OdsLastChange {
    param([object]$Project)
    $paths = @($Project.local, $Project.dest) | Where-Object { Test-Path -LiteralPath $_ }
    $latest = [datetime]::MinValue
    foreach ($p in $paths) {
        $f = Get-ChildItem -LiteralPath $p -Recurse -File -ErrorAction SilentlyContinue |
             Where-Object { $_.FullName -notmatch '\\\.git\\' } | Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1
        if ($f -and $f.LastWriteTimeUtc -gt $latest) { $latest = $f.LastWriteTimeUtc }
    }
    return $latest
}

function Update-OdsDeferCount {
    param([string]$Id, [hashtable]$Config)
    Edit-OdsMachineState -Mutate {
        param($s)
        $n = 1
        if ($null -ne $s.deferred.PSObject.Properties[$Id]) { $n = [int]$s.deferred.$Id + 1 }
        $s.deferred | Add-Member -NotePropertyName $Id -NotePropertyValue $n -Force
        if ($n -ge $Config.DeferEscalateCycles) {
            Write-OdsLog "ESCALATION: $Id deferred $n consecutive cycles — needs attention." 'ERROR'
        }
    }
}

function Write-OdsPending {
    param([object[]]$Undecided)
    $list = @($Undecided | ForEach-Object { [pscustomobject]@{ id=$_.id; name=$_.name; kind=$_.kind } })
    Write-OdsJson -Path $script:OdsPending -Object $list
}
#endregion

# ----------------------------------------------------------------------------
#region  Public mutations
# ----------------------------------------------------------------------------
function Pull-OdsProject {
    param([string]$Id, [hashtable]$Config)
    # clear tombstone if present
    $cat = Get-OdsCatalog
    if (@($cat.forgotten) -contains $Id) {
        $cat.forgotten = @($cat.forgotten | Where-Object { $_ -ne $Id })
        Save-OdsCatalog $cat
    }
    Set-OdsState -Id $Id -Status active
    $proj = @(Get-OdsProjects -Config $Config) | Where-Object { $_.id -eq $Id } | Select-Object -First 1
    if (-not $proj) { throw "Project '$Id' not found in available set." }
    Sync-OdsProject -Project $proj -Config $Config
}

function Unmap-OdsProject {
    param([string]$Id, [hashtable]$Config, [switch]$DeleteLocal)
    $proj = @(Get-OdsProjects -Config $Config) | Where-Object { $_.id -eq $Id } | Select-Object -First 1
    Set-OdsState -Id $Id -Status skip
    Invoke-OdsWithProjectLock -Id $Id -Body { Reset-OdsBaseline -Id $Id }
    if ($DeleteLocal -and $proj -and (Test-Path -LiteralPath $proj.local)) {
        if (Test-OdsIsProtectedRoot $proj.local) {
            throw "Refusing -DeleteLocal for '$Id': '$($proj.local)' is (or contains) a protected root."
        }
        Remove-Item -LiteralPath $proj.local -Recurse -Force
        Write-OdsLog "Removed local copy of $Id (OneDrive copy preserved)." 'INFO'
    }
    Write-OdsLog "Unmapped $Id on this machine (skip). OneDrive copy intact." 'INFO'
}

function Forget-OdsProject {
    param([string]$Id, [hashtable]$Config)
    $cat = Get-OdsCatalog
    $cat.entries = @($cat.entries | Where-Object { $_.id -ne $Id })
    if (@($cat.forgotten) -notcontains $Id) { $cat.forgotten = @($cat.forgotten) + $Id }
    Save-OdsCatalog $cat
    Edit-OdsMachineState -Mutate {
        param($s)
        $s.active = @($s.active | Where-Object { $_ -ne $Id })
        $s.skip   = @($s.skip   | Where-Object { $_ -ne $Id })
    }
    Write-OdsLog "Forgot $Id (tombstoned). OneDrive copy untouched; use -Pull to revive." 'INFO'
}

function Add-OdsWatchMapping {
    <# Persist a new watch entry (arbitrary dest chosen by the user). #>
    param([string]$Local, [string]$Dest, [hashtable]$Config)
    $od = Get-OdsOneDriveRoot; $up = $env:USERPROFILE.TrimEnd('\')
    # IsNullOrEmpty: Get-OdsRelUnder returns '' (not $null) when the folder IS the root,
    # which must also be refused or a project would map onto the whole root tree.
    $destRel = Get-OdsRelUnder -Full $Dest -Root $od
    if ([string]::IsNullOrEmpty($destRel)) { throw "Destination must be a folder UNDER OneDrive ($od)." }
    $localRel = Get-OdsRelUnder -Full $Local -Root $up
    if ([string]::IsNullOrEmpty($localRel)) { throw "Local folder must be a folder UNDER your profile ($up)." }
    if ((Test-OdsIsProtectedRoot $Local) -or (Test-OdsIsProtectedRoot $Dest)) { throw "Local or Dest is a protected root — refused." }
    if (Test-OdsOverlap $Local $Dest) { throw "Local and Dest overlap — refused (would self-sync)." }
    $cat = Get-OdsCatalog
    $cat.entries = @($cat.entries | Where-Object { $_.id -ne $destRel }) + [pscustomobject]@{
        id=$destRel; localRel=$localRel; destRel=$destRel; kind='watch'
    }
    Save-OdsCatalog $cat
    Set-OdsState -Id $destRel -Status active
    Write-OdsLog "Mapped watch project $localRel -> $destRel." 'INFO'
    return $destRel
}
#endregion

# ----------------------------------------------------------------------------
#region  Status, restore, self-update
# ----------------------------------------------------------------------------
function Get-OdsProjectStatus {
    param([hashtable]$Config)
    $state = Get-OdsMachineState
    $projects = @(Get-OdsProjects -Config $Config)
    foreach ($p in $projects) {
        $status = if ($state.active -contains $p.id) { 'active' }
                  elseif ($state.skip -contains $p.id) { 'skip' } else { 'undecided' }
        [pscustomobject]@{
            Id=$p.id; Name=$p.name; Kind=$p.kind; Git=$p.git; Status=$status
            Local=$p.local; LocalPresent=(Test-Path -LiteralPath $p.local)
            Conflicts=@(if (Test-Path -LiteralPath $p.local) { Get-OdsConflicts -Project $p } else { @() }).Count
        }
    }
}

function Get-OdsRunStamp {
    # Parse the UTC timestamp from an archive run-dir name (bare 'yyyyMMddTHHmmssZ',
    # or a 'seed-' / 'pre-restore-' prefix). Unparseable -> DateTime.MinValue.
    param([string]$Name)
    $core = $Name -replace '^(seed|pre-restore)-', ''
    $dt = [datetime]::MinValue
    $ok = [datetime]::TryParseExact($core, 'yyyyMMddTHHmmssZ', [Globalization.CultureInfo]::InvariantCulture,
            [Globalization.DateTimeStyles]::AssumeUniversal -bor [Globalization.DateTimeStyles]::AdjustToUniversal, [ref]$dt)
    if ($ok) { return $dt } else { return [datetime]::MinValue }
}

function Restore-OdsItem {
    <# Restore a file/subpath/whole project from the local version archive (G). #>
    param([string]$Id, [hashtable]$Config, [string]$At, [string]$File)
    if ($File -and ($File -match '(^|[\\/])\.\.([\\/]|$)')) { throw "Invalid -File '$File' ('..' is not allowed)." }
    $idHash = Get-OdsIdHash $Id
    $base = Join-Path $script:OdsVersionsDir $idHash
    if (-not (Test-Path -LiteralPath $base)) { throw "No local versions for '$Id'. Try OneDrive version history." }
    # Sort by PARSED timestamp, not raw name — else 'seed-*' sorts lexically ahead of
    # bare-timestamp backups and a plain restore returns the partial seed snapshot.
    $runs = @(Get-ChildItem -LiteralPath $base -Directory | Where-Object { $_.Name -notlike 'pre-restore-*' }) |
            Sort-Object @{ Expression = { Get-OdsRunStamp $_.Name } } -Descending
    if ($At) {
        $atDt = [datetime]::MinValue
        if (-not [datetime]::TryParse($At, [Globalization.CultureInfo]::InvariantCulture, [Globalization.DateTimeStyles]::AssumeLocal, [ref]$atDt)) {
            throw "Could not parse -At '$At' as a date/time."
        }
        $atUtc = $atDt.ToUniversalTime()
        $runs = @($runs | Where-Object { (Get-OdsRunStamp $_.Name) -le $atUtc })
    }
    $run = $runs | Select-Object -First 1
    if (-not $run) { throw "No archived version at/before '$At'." }
    $proj = @(Get-OdsProjects -Config $Config) | Where-Object { $_.id -eq $Id } | Select-Object -First 1
    if (-not $proj) { throw "Project '$Id' not found." }
    $src = if ($File) { Join-Path $run.FullName $File } else { $run.FullName }
    if (-not (Test-Path -LiteralPath $src)) { throw "Not in archive run $($run.Name): $File" }
    $dst = if ($File) { Join-Path $proj.local $File } else { $proj.local }

    # Back up current content before clobbering, so a wrong restore is undoable.
    if (Test-Path -LiteralPath $dst) {
        try {
            $preDir = Join-Path $base ('pre-restore-' + (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ'))
            $preTarget = if ($File) { Join-Path $preDir $File } else { $preDir }
            $ptd = Split-Path -Parent $preTarget
            if (-not (Test-Path -LiteralPath $ptd)) { New-Item -ItemType Directory -Path $ptd -Force | Out-Null }
            Copy-Item -LiteralPath $dst -Destination $preTarget -Recurse -Force
            Write-OdsLog "Backed up current '$Id' to $(Split-Path $preDir -Leaf) before restore." 'INFO'
        } catch { Write-OdsLog "Pre-restore backup of '$Id' failed: $($_.Exception.Message). Proceeding." 'WARN' }
    }

    Write-OdsLog "Restoring $Id $(if($File){$File}else{'(whole)'}) from $($run.Name)." 'INFO'
    Copy-Item -LiteralPath $src -Destination $dst -Recurse -Force
    Write-OdsEvent 'restore' @{ id=$Id; run=$run.Name; file=$File }
}

function Update-OdsAppFromSource {
    <#
      Run-local update launcher (§7). Compares the OneDrive app-src VERSION with the
      staged local copy; stages newer scripts ('auto') or returns 'available' ('notify').
      Returns: 'updated' | 'available' | 'current'.
    #>
    param([hashtable]$Config)
    $srcDir = Join-Path (Get-OdsToolDataDir) 'app-src'
    if (-not (Test-Path -LiteralPath $srcDir)) { return 'current' }
    $srcVer = (Get-Content -LiteralPath (Join-Path $srcDir 'VERSION') -Raw -ErrorAction SilentlyContinue).Trim()
    $localVerFile = Join-Path $script:OdsAppDir 'VERSION'
    $localVer = if (Test-Path -LiteralPath $localVerFile) { (Get-Content -LiteralPath $localVerFile -Raw).Trim() } else { '' }
    if ($srcVer -eq $localVer) { return 'current' }
    if ($Config.ToolUpdateMode -eq 'notify') { return 'available' }

    # Kill any lingering tray process so its files are not in use during the copy.
    $trayPattern = [regex]::Escape((Join-Path $script:OdsAppDir 'onedrive-sync-tray.ps1'))
    $trayProcs = Get-CimInstance Win32_Process -Filter "Name='powershell.exe' OR Name='pwsh.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -match $trayPattern }
    $hadTray = $trayProcs -as [bool]
    if ($trayProcs) { $trayProcs | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue } }
    if ($hadTray) { Start-Sleep -Milliseconds 500 }

    # Stage all updated files.
    if (-not (Test-Path -LiteralPath $script:OdsAppDir)) { New-Item -ItemType Directory -Path $script:OdsAppDir -Force | Out-Null }
    Get-ChildItem -LiteralPath $srcDir -File | ForEach-Object {
        Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $script:OdsAppDir $_.Name) -Force
    }
    Write-OdsLog "Staged tool update $localVer -> $srcVer." 'INFO'

    # Restart the tray if it was running.
    if ($hadTray) {
        $trayScript = Join-Path $script:OdsAppDir 'onedrive-sync-tray.ps1'
        Start-Process powershell.exe -ArgumentList @('-NoProfile', '-STA', '-WindowStyle', 'Hidden', '-ExecutionPolicy', 'Bypass', '-File', "`"$trayScript`"") -ErrorAction SilentlyContinue
    }

    return 'updated'
}
#endregion
