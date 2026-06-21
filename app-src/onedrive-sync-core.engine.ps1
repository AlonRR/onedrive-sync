# OneDrive sync core — engine (dot-sourced by onedrive-sync-core.ps1).
# Filters, gate, seed, bisync, verify, conflicts, divergence, versioning,
# reconcile, restore, and the run orchestrator.

# ----------------------------------------------------------------------------
#region  Tool location
# ----------------------------------------------------------------------------
function Get-OdsRclone {
    $local = Join-Path $script:OdsLocalRoot 'rclone.exe'
    if (Test-Path -LiteralPath $local) { return $local }
    $cmd = Get-Command rclone -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw "rclone.exe not found (expected at $local or on PATH). Run install-task.ps1."
}
function Get-OdsGit {
    $cmd = Get-Command git -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    throw "git not found on PATH. Run install-task.ps1."
}
function Invoke-OdsGit {
    param([string]$RepoDir, [string[]]$GitArgs)
    $git = Get-OdsGit
    $out = & $git -C $RepoDir @GitArgs 2>$null
    [pscustomobject]@{ Code = $LASTEXITCODE; Output = (($out | Out-String).TrimEnd("`r","`n")) }
}
#endregion

# ----------------------------------------------------------------------------
#region  Filter generation (§2, E1/E53/E57/E74)
# ----------------------------------------------------------------------------
function ConvertTo-OdsFilterPath { param([string]$p) ($p -replace '\\','/') }

function Test-OdsMatchesExclude {
    param([string]$RelPath, [string[]]$ExcludeDirs, [string[]]$ExcludeFiles)
    $segs = ($RelPath -replace '\\','/').Split('/')
    foreach ($s in $segs[0..($segs.Count-2)]) { if ($ExcludeDirs -contains $s) { return $true } }
    $leaf = $segs[-1]
    foreach ($pat in $ExcludeFiles) { if ($leaf -like $pat) { return $true } }
    return $false
}

function New-OdsFilterFile {
    <#
      Build a per-repo rclone filter file (first-match-wins ordering). For plain
      (non-git) folders pass -Git:$false to skip all git rules.
      Returns the filter file path.
    #>
    param([object]$Project, [hashtable]$Config)

    $hashDir = Get-OdsWorkdir -Id $Project.id
    if (-not (Test-Path -LiteralPath $hashDir)) { New-Item -ItemType Directory -Path $hashDir -Force | Out-Null }
    $filterFile = Join-Path $hashDir 'filter.txt'
    $lines = [System.Collections.Generic.List[string]]::new()

    if ($Project.git) {
        # 1) volatile/local-only git metadata (E57/E6/E74)
        foreach ($g in '/.git/index','/.git/logs/**','/.git/FETCH_HEAD','/.git/ORIG_HEAD',
                       '/.git/COMMIT_EDITMSG','/.git/**/*.lock','/.git/index.lock','/.git/*.tmp') {
            $lines.Add("- $g")
        }
        # 2) sync the rest of git history
        $lines.Add('+ /.git/**')

        # 3) tracked files that WOULD be excluded -> always include (E1)
        $tracked = @()
        if (Test-Path -LiteralPath (Join-Path $Project.local '.git')) {
            $r = Invoke-OdsGit -RepoDir $Project.local -GitArgs @('ls-files','-z')
            if ($r.Code -eq 0 -and $r.Output) { $tracked = $r.Output.Split([char]0) | Where-Object { $_ } }
        }
        foreach ($t in $tracked) {
            if (Test-OdsMatchesExclude -RelPath $t -ExcludeDirs $Config.ExcludeDirs -ExcludeFiles $Config.ExcludeFiles) {
                $lines.Add("+ /" + (ConvertTo-OdsFilterPath $t))
            }
        }
    }

    # 4) excluded dirs (anywhere)
    foreach ($d in $Config.ExcludeDirs) { $lines.Add("- $d/**") }

    # 5) allow-list (after dir-excludes so it never reaches into excluded dirs)
    foreach ($a in $Config.SyncAnywayList) { $lines.Add("+ $a") }

    # 6) excluded file patterns
    foreach ($f in $Config.ExcludeFiles) { $lines.Add("- $f") }

    # 7) gitignore-derived excludes (coarse, E53/E60)
    if ($Project.git -and (Test-Path -LiteralPath (Join-Path $Project.local '.git'))) {
        $r = Invoke-OdsGit -RepoDir $Project.local -GitArgs @('ls-files','--others','--ignored','--exclude-standard','--directory','-z')
        if ($r.Code -eq 0 -and $r.Output) {
            foreach ($p in ($r.Output.Split([char]0) | Where-Object { $_ })) {
                $fp = ConvertTo-OdsFilterPath $p
                if ($fp.EndsWith('/')) { $lines.Add("- /$fp**") } else { $lines.Add("- /$fp") }
            }
        }
    }

    # default: include everything else.
    $lines.Add('+ **')

    $content = ($lines -join "`n")
    $existing = if (Test-Path -LiteralPath $filterFile) { Get-Content -LiteralPath $filterFile -Raw } else { '' }
    $changed = ($content.TrimEnd() -ne $existing.TrimEnd())
    if ($changed) {
        Set-Content -LiteralPath $filterFile -Value $content -Encoding utf8
    }
    return [pscustomobject]@{ Path = $filterFile; Changed = $changed }
}

function Get-OdsIdHash {
    param([string]$Id)
    $md5 = [System.Security.Cryptography.MD5]::Create()
    $bytes = $md5.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($Id.ToLowerInvariant()))
    return ([System.BitConverter]::ToString($bytes) -replace '-','').Substring(0,16)
}

function Get-OdsWorkdir {
    # The bisync workdir (listing + filter) for a project id, keyed by id-hash.
    param([Parameter(Mandatory)][string]$Id)
    Join-Path $script:OdsBisyncDir (Get-OdsIdHash $Id)
}

function Reset-OdsBaseline {
    <#
      Drop a project's bisync baseline so the next run does a clean --resync.
      Default removes the whole workdir (listing + filter); -ListingOnly keeps the
      workdir/filter but wipes only the .lst listings.
    #>
    param([Parameter(Mandatory)][string]$Id, [switch]$ListingOnly)
    $wd = Get-OdsWorkdir -Id $Id
    if (-not (Test-Path -LiteralPath $wd)) { return }
    if ($ListingOnly) {
        Get-ChildItem -LiteralPath $wd -Filter '*.lst' -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
    } else {
        Remove-Item -LiteralPath $wd -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Invoke-OdsWithProjectLock {
    <#
      Serialize an operation on ONE project's bisync pair/workdir (a manual Sync Now
      racing the scheduled run) WITHOUT blocking other projects. Lock file sits beside
      the workdir so Reset-OdsBaseline doesn't disturb it; atomic File.Open(CreateNew),
      break a presumed-stale lock past the timeout rather than deadlock.
    #>
    param([Parameter(Mandatory)][string]$Id, [Parameter(Mandatory)][scriptblock]$Body, [int]$TimeoutMs = 600000)
    if (-not (Test-Path -LiteralPath $script:OdsBisyncDir)) { New-Item -ItemType Directory -Path $script:OdsBisyncDir -Force | Out-Null }
    $lock = Join-Path $script:OdsBisyncDir ((Get-OdsIdHash $Id) + '.lock')
    $handle = $null
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($null -eq $handle) {
        try {
            $handle = [System.IO.File]::Open($lock, [System.IO.FileMode]::CreateNew,
                                             [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
        } catch {
            if ($sw.ElapsedMilliseconds -gt $TimeoutMs) {
                Write-OdsLog "Project '$Id' lock held >$TimeoutMs ms; breaking presumed-stale lock." 'WARN'
                Remove-Item -LiteralPath $lock -Force -ErrorAction SilentlyContinue
                break
            }
            Start-Sleep -Milliseconds 100
        }
    }
    try { & $Body } finally {
        if ($handle) { try { $handle.Close(); $handle.Dispose() } catch {} }
        Remove-Item -LiteralPath $lock -Force -ErrorAction SilentlyContinue
    }
}

#endregion

# ----------------------------------------------------------------------------
#region  Pre-sync gate (E4/E6)
# ----------------------------------------------------------------------------
function Test-OdsGitQuiesced {
    param([string]$RepoLocal, [int]$StableSeconds)
    $gitDir = Join-Path $RepoLocal '.git'
    if (-not (Test-Path -LiteralPath $gitDir)) { return $true }  # plain or empty
    # any lock present?
    if (Get-ChildItem -LiteralPath $gitDir -Recurse -Filter '*.lock' -ErrorAction SilentlyContinue | Select-Object -First 1) {
        return $false
    }
    # recently modified?
    $cut = (Get-Date).AddSeconds(-$StableSeconds)
    $recent = Get-ChildItem -LiteralPath $gitDir -Recurse -File -ErrorAction SilentlyContinue |
              Where-Object { $_.LastWriteTime -gt $cut } | Select-Object -First 1
    return -not $recent
}
function Test-OdsTreeStable {
    param([string]$Dir, [int]$StableSeconds)
    if (-not (Test-Path -LiteralPath $Dir)) { return $true }
    $cut = (Get-Date).AddSeconds(-$StableSeconds)
    $recent = Get-ChildItem -LiteralPath $Dir -Recurse -File -ErrorAction SilentlyContinue |
              Where-Object { $_.LastWriteTime -gt $cut } | Select-Object -First 1
    return -not $recent
}
function Test-OdsGate {
    param([object]$Project, [hashtable]$Config)
    $s = $Config.IdleStabilitySeconds
    if (-not (Test-OdsTreeStable -Dir $Project.dest -StableSeconds $s)) {
        return [pscustomobject]@{ Ok=$false; Reason='onedrive-busy'; Transient=$true }
    }
    if ($Project.git -and -not (Test-OdsGitQuiesced -RepoLocal $Project.local -StableSeconds $s)) {
        return [pscustomobject]@{ Ok=$false; Reason='git-active'; Transient=$true }
    }
    return [pscustomobject]@{ Ok=$true }
}
#endregion

# ----------------------------------------------------------------------------
#region  First-run seed (E5/E47)
# ----------------------------------------------------------------------------
function Get-OdsFileHash { param([string]$Path) (Get-FileHash -LiteralPath $Path -Algorithm SHA256 -ErrorAction Stop).Hash }

function Invoke-OdsSeed {
    <# Newest-wins reconcile when both sides are non-empty and no baseline yet. #>
    param([object]$Project, [hashtable]$Config)
    $local = $Project.local; $dest = $Project.dest
    if (-not (Test-Path -LiteralPath $local) -or -not (Test-Path -LiteralPath $dest)) { return }
    $localFiles = Get-ChildItem -LiteralPath $local -Recurse -File -ErrorAction SilentlyContinue
    $destFiles  = Get-ChildItem -LiteralPath $dest  -Recurse -File -ErrorAction SilentlyContinue
    if (-not $localFiles -or -not $destFiles) { return }

    $archive = Join-Path (Join-Path $script:OdsVersionsDir (Get-OdsIdHash $Project.id)) ('seed-' + (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ'))
    foreach ($lf in $localFiles) {
        $rel = $lf.FullName.Substring($local.Length).TrimStart('\')
        $df = Join-Path $dest $rel
        if (-not (Test-Path -LiteralPath $df)) { continue }
        try {
            if ((Get-OdsFileHash $lf.FullName) -eq (Get-OdsFileHash $df)) { continue } # identical
        } catch { continue }
        $dfTime = (Get-Item -LiteralPath $df).LastWriteTimeUtc
        $skew = [math]::Abs(($lf.LastWriteTimeUtc - $dfTime).TotalHours)
        if ($skew -gt 48) { Write-OdsLog "Large mtime gap on '$rel' during seed ($([int]$skew)h) — check machine clocks." 'WARN' }
        if ($lf.LastWriteTimeUtc -ge $dfTime) { $loser = $df; $loserRoot = $dest }
        else { $loser = $lf.FullName; $loserRoot = $local }
        Save-OdsArchiveCopy -SourceFile $loser -RepoRoot $loserRoot -ArchiveDir $archive
    }
    Write-OdsEvent 'seed' @{ id = $Project.id }
}
function Save-OdsArchiveCopy {
    param([string]$SourceFile, [string]$RepoRoot, [string]$ArchiveDir)
    $rel = $SourceFile.Substring($RepoRoot.Length).TrimStart('\')
    $target = Join-Path $ArchiveDir $rel
    $td = Split-Path -Parent $target
    if (-not (Test-Path -LiteralPath $td)) { New-Item -ItemType Directory -Path $td -Force | Out-Null }
    Copy-Item -LiteralPath $SourceFile -Destination $target -Force
}
#endregion

# ----------------------------------------------------------------------------
#region  Bisync (§3) + post-sync verify (E6)
# ----------------------------------------------------------------------------
function Test-OdsBaselineExists {
    param([object]$Project)
    $wd = Get-OdsWorkdir -Id $Project.id
    # bisync stores .lst listing files in the workdir once a baseline is set.
    return [bool](Get-ChildItem -LiteralPath $wd -Filter '*.lst' -ErrorAction SilentlyContinue | Select-Object -First 1)
}

function Invoke-OdsBisync {
    param([object]$Project, [hashtable]$Config, [switch]$Resync, [switch]$DryRun, [switch]$Force)

    $rclone = Get-OdsRclone
    $idHash = Get-OdsIdHash $Project.id
    $wd = Join-Path $script:OdsBisyncDir $idHash
    if (-not (Test-Path -LiteralPath $wd)) { New-Item -ItemType Directory -Path $wd -Force | Out-Null }
    $filter = New-OdsFilterFile -Project $Project -Config $Config
    $stamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
    $backup = Join-Path (Join-Path $script:OdsVersionsDir $idHash) $stamp

    $machState    = Get-OdsMachineState
    $compareState = $machState.compare
    $mode = if ($null -ne $compareState.PSObject.Properties[$Project.id]) { $compareState.$($Project.id) } else { $Config.CompareMode }
    $compare = if ($mode -eq 'checksum') { 'size,checksum' } else { 'size,modtime' }
    $mdState   = $machState.maxDelete
    $maxDelete = if ($null -ne $mdState.PSObject.Properties[$Project.id]) { [int]$mdState.$($Project.id) } else { $Config.MaxDeletePercent }

    if (-not (Test-Path -LiteralPath $Project.local)) { New-Item -ItemType Directory -Path $Project.local -Force | Out-Null }
    if (-not (Test-Path -LiteralPath $Project.dest))  { New-Item -ItemType Directory -Path $Project.dest  -Force | Out-Null }

    $doResync = [bool]$Resync -or $filter.Changed

    $rcArgs = @(
        'bisync', $Project.local, $Project.dest,
        '--filters-file', $filter.Path,
        '--conflict-resolve','none',
        '--conflict-suffix', ("conflict-{0}-{1}" -f $env:COMPUTERNAME, $stamp),
        '--backup-dir1', $backup,
        '--max-delete', $maxDelete,
        '--transfers', $Config.RcloneTransfers,
        '--compare', $compare,
        '--resilient','--recover',
        '--workdir', $wd,
        '--log-file', $script:OdsLogFile,
        '--log-level','INFO'
    )
    # Plain --resync == --resync-mode path1 (local always wins), which silently
    # overwrites a newer OneDrive edit whenever a resync is forced (e.g. a routine
    # filter/.gitignore change). --resync-mode newer keeps the newer side instead.
    if ($doResync) { $rcArgs += @('--resync', '--resync-mode', 'newer') }
    if ($DryRun) { $rcArgs += '--dry-run' }
    if ($Force)  { $rcArgs += '--force' }

    Write-OdsLog "bisync $($Project.id) [$($mode)]$(if($doResync){' resync'})$(if($DryRun){' dry-run'})" 'INFO'
    $code = Invoke-OdsWithProjectLock -Id $Project.id -Body {
        & $rclone @rcArgs | Out-Null
        $LASTEXITCODE
    }
    Write-OdsEvent 'bisync' @{ id=$Project.id; code=$code; resync=$doResync; dryrun=[bool]$DryRun }
    return $code
}

function Test-OdsGitIntegrity {
    <# Post-sync verify (E6/E75). Returns $true if .git is consistent (or unborn). #>
    param([object]$Project)
    if (-not $Project.git) { return $true }
    $gitDir = Join-Path $Project.local '.git'
    if (-not (Test-Path -LiteralPath $gitDir)) { return $true }
    $head = Invoke-OdsGit -RepoDir $Project.local -GitArgs @('rev-parse','--verify','--quiet','HEAD')
    if ($head.Code -ne 0) { return $true }   # unborn HEAD (no commits) is valid (E54)
    $fsck = Invoke-OdsGit -RepoDir $Project.local -GitArgs @('fsck','--connectivity-only')
    if ($fsck.Code -ne 0) {
        Write-OdsLog "git fsck failed for $($Project.id): $($fsck.Output)" 'ERROR'
        return $false
    }
    return $true
}
#endregion

# ----------------------------------------------------------------------------
#region  Conflict scan (§4) + divergence guard (E2)
# ----------------------------------------------------------------------------
function Get-OdsConflicts {
    param([object]$Project)
    if (-not (Test-Path -LiteralPath $Project.local)) { return @() }
    $cn = [regex]::Escape($env:COMPUTERNAME)
    $gitSeg = [IO.Path]::DirectorySeparatorChar + '.git' + [IO.Path]::DirectorySeparatorChar
    @(Get-ChildItem -LiteralPath $Project.local -Recurse -File -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -notlike "*$gitSeg*" } |
        Where-Object { $_.Name -like '*.conflict-*' -or $_.Name -match "-$cn\.[^.]+$" } |
        Select-Object -ExpandProperty FullName -Unique)
}

function Resolve-OdsDivergence {
    <#
      If a .git ref conflict is detected, do not accept it: compare resolved tips,
      and on real two-tip divergence run a local git merge (fast-forward or surface
      conflict); tag orphan tips so nothing is lost (E2/E56).
    #>
    param([object]$Project)
    if (-not $Project.git) { return }
    # Detect ref conflict copies bisync may have made under .git/refs.
    $refConf = Get-ChildItem -LiteralPath (Join-Path $Project.local '.git\refs') -Recurse -File -ErrorAction SilentlyContinue |
               Where-Object { $_.Name -match 'conflict-' }
    $branch = (Invoke-OdsGit -RepoDir $Project.local -GitArgs @('symbolic-ref','--quiet','--short','HEAD')).Output.Trim()
    if (-not $branch) { return } # detached/unborn (E55)
    if (-not $refConf) { return } # no divergence surfaced

    Write-OdsLog "Divergence detected on $($Project.id) branch '$branch'; reconciling via git." 'WARN'
    # Fetch the OneDrive copy as a remote and merge.
    $tmpRemote = $Project.dest
    Invoke-OdsGit -RepoDir $Project.local -GitArgs @('fetch', $tmpRemote, $branch) | Out-Null
    $merge = Invoke-OdsGit -RepoDir $Project.local -GitArgs @('merge','--no-edit','FETCH_HEAD')
    if ($merge.Code -ne 0) {
        $tag = "ods-orphan-$((Get-Date).ToString('yyyyMMddHHmmss'))"
        Invoke-OdsGit -RepoDir $Project.local -GitArgs @('tag', $tag, 'FETCH_HEAD') | Out-Null
        Invoke-OdsGit -RepoDir $Project.local -GitArgs @('merge','--abort') | Out-Null
        Write-OdsLog "Auto-merge failed on $($Project.id); tagged other tip as $tag for manual resolution." 'ERROR'
        Write-OdsEvent 'divergence' @{ id=$Project.id; result='manual'; tag=$tag }
    } else {
        Write-OdsEvent 'divergence' @{ id=$Project.id; result='merged' }
    }
    foreach ($rc in $refConf) { Remove-Item -LiteralPath $rc.FullName -Force -ErrorAction SilentlyContinue }
}
#endregion

# ----------------------------------------------------------------------------
#region  Versioning prune (§5/E19)
# ----------------------------------------------------------------------------
function Invoke-OdsVersionPrune {
    param([hashtable]$Config)
    if (-not (Test-Path -LiteralPath $script:OdsVersionsDir)) { return }
    $cut = (Get-Date).AddDays(-$Config.VersionRetentionDays)
    # age-based
    foreach ($d in (Get-ChildItem -LiteralPath $script:OdsVersionsDir -Directory -ErrorAction SilentlyContinue)) {
        foreach ($run in (Get-ChildItem -LiteralPath $d.FullName -Directory -ErrorAction SilentlyContinue)) {
            if ($run.LastWriteTime -lt $cut) { Remove-Item -LiteralPath $run.FullName -Recurse -Force -ErrorAction SilentlyContinue }
        }
    }
    # size-cap (oldest first)
    $allRuns = Get-ChildItem -LiteralPath $script:OdsVersionsDir -Directory -ErrorAction SilentlyContinue |
               ForEach-Object { Get-ChildItem -LiteralPath $_.FullName -Directory -ErrorAction SilentlyContinue } |
               Sort-Object LastWriteTime
    $maxBytes = [int64]$Config.VersionMaxGB * 1GB
    $total = ($allRuns | ForEach-Object { (Get-ChildItem -LiteralPath $_.FullName -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum } | Measure-Object -Sum).Sum
    $i = 0
    while ($total -gt $maxBytes -and $i -lt $allRuns.Count) {
        $sz = (Get-ChildItem -LiteralPath $allRuns[$i].FullName -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum
        Remove-Item -LiteralPath $allRuns[$i].FullName -Recurse -Force -ErrorAction SilentlyContinue
        $total -= [int64]$sz; $i++
    }
}
#endregion

. (Join-Path $PSScriptRoot 'onedrive-sync-core.run.ps1')
