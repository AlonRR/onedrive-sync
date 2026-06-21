<#
.SYNOPSIS
    OneDrive 2-way sync — shared core module.

.DESCRIPTION
    All sync logic lives here and is reused by the CLI (onedrive-sync.ps1), the
    tray/GUI (onedrive-sync-tray.ps1) and the installer. Dot-source this file then
    call the public functions (Invoke-OdsRun, Get-OdsProjects, Sync-OdsProject,
    Pull-OdsProject, Unmap-OdsProject, Forget-OdsProject, Restore-OdsItem, ...).

    Design reference: the approved plan. Project model: a folder is a project iff
    it contains .git (git:true) OR is an explicit non-git $PlainFolders entry
    (git:false). The mirroring law maps OneDrive\<rel> <-> %USERPROFILE%\<rel>.
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Native commands (git, rclone) signal status via EXIT CODE — we inspect those
# ourselves (rclone 1-7 = partial success, etc.). Do NOT let a non-zero exit or a
# stderr warning throw. (PS 7.3+ only; the variable is absent on Windows PowerShell.)
if (Get-Variable -Name PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue) {
    $PSNativeCommandUseErrorActionPreference = $false
}

# ============================================================================
#region  Paths & environment
# ============================================================================

# Local (per-machine) data root — never synced.
$script:OdsLocalRoot   = Join-Path $env:LOCALAPPDATA 'onedrive-sync'
$script:OdsAppDir      = Join-Path $script:OdsLocalRoot 'app'
$script:OdsBisyncDir   = Join-Path $script:OdsLocalRoot 'bisync'
$script:OdsVersionsDir = Join-Path $script:OdsLocalRoot 'versions'
$script:OdsEventsDir   = Join-Path $script:OdsLocalRoot 'events'
$script:OdsLogDir      = Join-Path $script:OdsLocalRoot 'logs'
$script:OdsLockFile    = Join-Path $script:OdsLocalRoot '.lock'
$script:OdsMachineState= Join-Path $script:OdsLocalRoot 'machine-state.json'
$script:OdsStateLock   = (Join-Path $script:OdsLocalRoot 'machine-state.json') + '.lock'
$script:OdsPending     = Join-Path $script:OdsLocalRoot 'pending.json'
$script:OdsLogFile     = Join-Path $script:OdsLogDir 'sync.log'

# OneDrive (shared) roots.
function Get-OdsOneDriveRoot {
    if (-not $env:OneDriveConsumer) {
        throw "`$env:OneDriveConsumer is not set — is the personal OneDrive client running and signed in?"
    }
    return $env:OneDriveConsumer.TrimEnd('\')
}
function Get-OdsToolDataDir {
    Join-Path (Get-OdsOneDriveRoot) 'Tools\onedrive-sync'
}
function Get-OdsMappingsPath { Join-Path (Get-OdsToolDataDir) 'mappings.json' }

function Initialize-OdsDirs {
    foreach ($d in @($script:OdsLocalRoot, $script:OdsAppDir, $script:OdsBisyncDir,
                     $script:OdsVersionsDir, $script:OdsEventsDir, $script:OdsLogDir)) {
        if (-not (Test-Path -LiteralPath $d)) { New-Item -ItemType Directory -Path $d -Force | Out-Null }
    }
    $tool = Get-OdsToolDataDir
    if (-not (Test-Path -LiteralPath $tool)) { New-Item -ItemType Directory -Path $tool -Force | Out-Null }
}

#endregion

# ============================================================================
#region  Config
# ============================================================================

function Import-OdsConfig {
    <# Loads sync-config.ps1 (and an optional local override) into a hashtable. #>
    param([string]$ConfigPath)

    if (-not $ConfigPath) { $ConfigPath = Join-Path $PSScriptRoot 'sync-config.ps1' }
    if (-not (Test-Path -LiteralPath $ConfigPath)) {
        throw "Config not found: $ConfigPath"
    }
    . $ConfigPath
    $override = Join-Path $script:OdsLocalRoot 'sync-config.local.ps1'
    if (Test-Path -LiteralPath $override) { . $override }

    $cfg = @{}
    foreach ($name in 'ProjectParents','WatchRoots','PlainFolders','ExcludeDirs','ExcludeFiles',
                      'SyncAnywayList','VersionRetentionDays','VersionMaxGB','MaxDeletePercent',
                      'IdleStabilitySeconds','CompareMode','NoiseThreshold','QuietRunsToRevert',
                      'RetryMaxAttempts','RetryBackoff','RetryMaxWaitSeconds','DeferEscalateCycles',
                      'RcloneTransfers','ToolUpdateMode','RunTimeBudget') {
        if (Get-Variable -Name $name -Scope Local -ErrorAction SilentlyContinue) {
            $cfg[$name] = (Get-Variable -Name $name -Scope Local).Value
        }
    }
    # Defaults for anything missing.
    $defaults = @{
        ProjectParents=@(); WatchRoots=@(); PlainFolders=@(); ExcludeDirs=@(); ExcludeFiles=@()
        SyncAnywayList=@(); VersionRetentionDays=30; VersionMaxGB=5; MaxDeletePercent=25
        IdleStabilitySeconds=60; CompareMode='modtime'; NoiseThreshold=3; QuietRunsToRevert=10
        RetryMaxAttempts=4; RetryBackoff=@(5,10,20); RetryMaxWaitSeconds=120; DeferEscalateCycles=5
        RcloneTransfers=4; ToolUpdateMode='auto'; RunTimeBudget=1500
    }
    foreach ($k in $defaults.Keys) { if (-not $cfg.ContainsKey($k)) { $cfg[$k] = $defaults[$k] } }
    return $cfg
}

#endregion

# ============================================================================
#region  Logging & audit (C)
# ============================================================================

function Write-OdsLog {
    param([string]$Message, [ValidateSet('INFO','WARN','ERROR')] [string]$Level = 'INFO')
    Initialize-OdsDirs
    $ts = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    $line = "$ts [$Level] $Message"
    Add-Content -LiteralPath $script:OdsLogFile -Value $line -Encoding utf8
    if ($Level -eq 'ERROR') { Write-Host $line -ForegroundColor Red }
    elseif ($Level -eq 'WARN') { Write-Host $line -ForegroundColor Yellow }
    else { Write-Verbose $line }
    # rotate at 10MB
    $fi = Get-Item -LiteralPath $script:OdsLogFile -ErrorAction SilentlyContinue
    if ($fi -and $fi.Length -gt 10MB) {
        Move-Item -LiteralPath $script:OdsLogFile -Destination "$($script:OdsLogFile).1" -Force
    }
}

function Write-OdsEvent {
    <# Structured per-run event for the audit log (JSONL). #>
    param([string]$Event, [hashtable]$Data = @{})
    Initialize-OdsDirs
    $rec = [ordered]@{
        ts    = (Get-Date).ToUniversalTime().ToString('o')
        event = $Event
        machine = $env:COMPUTERNAME
    }
    foreach ($k in $Data.Keys) { $rec[$k] = $Data[$k] }
    $file = Join-Path $script:OdsEventsDir ("{0}.jsonl" -f (Get-Date).ToUniversalTime().ToString('yyyy-MM-dd'))
    Add-Content -LiteralPath $file -Value ($rec | ConvertTo-Json -Compress -Depth 6) -Encoding utf8
}

#endregion

# ============================================================================
#region  Lock (E12)
# ============================================================================

function Enter-OdsLock {
    param([int]$MaxAgeMinutes = 60)
    Initialize-OdsDirs
    # Atomic acquire: File.Open(CreateNew) fails if the lock exists, so two runs
    # starting together cannot both win (no Test-Path-then-write TOCTOU). A stale
    # lock (dead pid / too old / unreadable) is broken and the create retried once.
    for ($attempt = 0; $attempt -lt 2; $attempt++) {
        try {
            $fs = [System.IO.File]::Open($script:OdsLockFile, [System.IO.FileMode]::CreateNew,
                                         [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
            try {
                $bytes = [System.Text.Encoding]::UTF8.GetBytes((@{ pid = $PID; ts = (Get-Date).ToString('o') } | ConvertTo-Json -Compress))
                $fs.Write($bytes, 0, $bytes.Length)
            } finally { $fs.Close(); $fs.Dispose() }
            return $true
        } catch {
            $break = $false
            try {
                $lock  = Get-Content -LiteralPath $script:OdsLockFile -Raw -ErrorAction Stop | ConvertFrom-Json
                $alive = $false
                if ($lock.PSObject.Properties['pid'] -and $lock.pid) { $alive = [bool](Get-Process -Id ([int]$lock.pid) -ErrorAction SilentlyContinue) }
                $age = if ($lock.PSObject.Properties['ts'] -and $lock.ts) { (Get-Date) - ([datetime]$lock.ts) } else { [timespan]::MaxValue }
                if ($alive -and $age.TotalMinutes -lt $MaxAgeMinutes) {
                    Write-OdsLog "Another run holds the lock (pid $($lock.pid)); exiting." 'WARN'
                    return $false
                }
                $pidShown = if ($lock.PSObject.Properties['pid']) { $lock.pid } else { '?' }
                Write-OdsLog "Breaking stale lock (pid $pidShown, age $([int]$age.TotalMinutes)m)." 'WARN'
                $break = $true
            } catch {
                Write-OdsLog "Unreadable lock file; breaking it." 'WARN'
                $break = $true
            }
            if (-not $break) { return $false }
            Remove-Item -LiteralPath $script:OdsLockFile -Force -ErrorAction SilentlyContinue
        }
    }
    return $false
}
function Exit-OdsLock {
    if (Test-Path -LiteralPath $script:OdsLockFile) {
        Remove-Item -LiteralPath $script:OdsLockFile -Force -ErrorAction SilentlyContinue
    }
}

#endregion

# ============================================================================
#region  Atomic JSON state I/O (E41)
# ============================================================================

function Read-OdsJson {
    param([string]$Path, $Default)
    if (-not (Test-Path -LiteralPath $Path)) { return $Default }
    try {
        $raw = Get-Content -LiteralPath $Path -Raw -Encoding utf8
        if ([string]::IsNullOrWhiteSpace($raw)) { return $Default }
        return ($raw | ConvertFrom-Json)
    } catch {
        Write-OdsLog "Corrupt JSON at $Path ($($_.Exception.Message)); using default." 'WARN'
        return $Default
    }
}
function Write-OdsJson {
    param([string]$Path, $Object)
    $dir = Split-Path -Parent $Path
    if (-not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
    $tmp = "$Path.tmp.$PID"
    # Use -InputObject to avoid PS 5.1 pipeline-unwrapping empty arrays to no output.
    $json = ConvertTo-Json -InputObject $Object -Depth 8 -ErrorAction SilentlyContinue
    if ([string]::IsNullOrEmpty($json)) { $json = if ($null -eq $Object) { 'null' } elseif ($Object -is [array]) { '[]' } else { '{}' } }
    try {
        [System.IO.File]::WriteAllText($tmp, $json, [System.Text.Utf8Encoding]::new($false))
        if (Test-Path -LiteralPath $Path) {
            [System.IO.File]::Replace($tmp, $Path, $null)   # atomic in-place replace (no delete+rename gap)
        } else {
            [System.IO.File]::Move($tmp, $Path)             # atomic create
        }
    } catch {
        Move-Item -LiteralPath $tmp -Destination $Path -Force   # best-effort fallback
    } finally {
        if (Test-Path -LiteralPath $tmp) { Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue }
    }
    # Sweep orphaned temps left by crashed / force-killed writers (older than 2 min,
    # so a concurrent writer's in-flight temp is never touched).
    try {
        $leaf = Split-Path -Leaf $Path
        $cut = (Get-Date).AddMinutes(-2)
        Get-ChildItem -LiteralPath $dir -Filter "$leaf.tmp.*" -File -ErrorAction SilentlyContinue |
            Where-Object { $_.LastWriteTime -lt $cut } | Remove-Item -Force -ErrorAction SilentlyContinue
    } catch {}
}

#endregion

# ============================================================================
#region  Catalog (mappings.json) + tombstones (E3)
# ============================================================================

function Get-OdsCatalog {
    $obj = Read-OdsJson -Path (Get-OdsMappingsPath) -Default ([pscustomobject]@{ entries=@(); forgotten=@() })
    # Tolerate the older bare-array shape.
    if ($obj -is [array]) { $obj = [pscustomobject]@{ entries = $obj; forgotten=@() } }
    if ($null -eq $obj.PSObject.Properties['entries'])   { $obj | Add-Member entries   @() -Force }
    if ($null -eq $obj.PSObject.Properties['forgotten']) { $obj | Add-Member forgotten @() -Force }
    return $obj
}
function Save-OdsCatalog {
    param($Catalog)
    # Merge any OneDrive conflict copies first (union by id).
    $dir = Get-OdsToolDataDir
    $conflicts = Get-ChildItem -LiteralPath $dir -Filter 'mappings-*.json' -ErrorAction SilentlyContinue
    foreach ($c in $conflicts) {
        try {
            $other = Get-Content -LiteralPath $c.FullName -Raw | ConvertFrom-Json
            if ($other -is [array]) { $other = [pscustomobject]@{ entries=$other; forgotten=@() } }
            $Catalog = Merge-OdsCatalog $Catalog $other
            Remove-Item -LiteralPath $c.FullName -Force
            Write-OdsLog "Merged catalog conflict copy $($c.Name)." 'WARN'
        } catch { Write-OdsLog "Could not merge $($c.Name): $($_.Exception.Message)" 'WARN' }
    }
    Write-OdsJson -Path (Get-OdsMappingsPath) -Object $Catalog
}
function Merge-OdsCatalog {
    param($A, $B)
    $byId = @{}
    foreach ($e in @($A.entries) + @($B.entries)) { if ($e) { $byId[$e.id] = $e } }
    $forgotten = @(@($A.forgotten) + @($B.forgotten) | Where-Object { $_ } | Sort-Object -Unique)
    [pscustomobject]@{ entries = @($byId.Values); forgotten = $forgotten }
}

#endregion

# ============================================================================
#region  Per-machine state (machine-state.json)
# ============================================================================

function Get-OdsMachineState {
    # compare/deferred/maxDelete must be PSCustomObject, not @{}: on a fresh machine the
    # Default is returned verbatim and an @{} hashtable silently drops Add-Member
    # NotePropertyName writes (the first per-project setting set).
    $s = Read-OdsJson -Path $script:OdsMachineState -Default ([pscustomobject]@{ active=@(); skip=@(); compare=[pscustomobject]@{}; deferred=[pscustomobject]@{} })
    foreach ($p in 'active','skip') { if ($null -eq $s.PSObject.Properties[$p]) { $s | Add-Member $p @() -Force } }
    foreach ($p in 'compare','deferred','maxDelete') { if ($null -eq $s.PSObject.Properties[$p]) { $s | Add-Member $p ([pscustomobject]@{}) -Force } }
    return $s
}
function Save-OdsMachineState { param($State) Write-OdsJson -Path $script:OdsMachineState -Object $State }

function Edit-OdsMachineState {
    <#
      Read-modify-write machine-state.json under a cross-process atomic file lock so
      the tray (its own process) and a scheduled run cannot clobber each other.
      Acquire via File.Open(CreateNew) (no Test-Path TOCTOU); a presumed-stale lock
      past the timeout is broken and we proceed best-effort rather than crash.
    #>
    param([Parameter(Mandatory)][scriptblock]$Mutate, [int]$TimeoutMs = 5000)
    Initialize-OdsDirs
    $handle = $null
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($null -eq $handle) {
        try {
            $handle = [System.IO.File]::Open($script:OdsStateLock, [System.IO.FileMode]::CreateNew,
                                             [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
        } catch {
            if ($sw.ElapsedMilliseconds -gt $TimeoutMs) {
                Write-OdsLog "machine-state lock held >$TimeoutMs ms; breaking presumed-stale lock." 'WARN'
                Remove-Item -LiteralPath $script:OdsStateLock -Force -ErrorAction SilentlyContinue
                break
            }
            Start-Sleep -Milliseconds 40
        }
    }
    try {
        $s = Get-OdsMachineState
        & $Mutate $s
        Save-OdsMachineState $s
    } finally {
        if ($handle) { try { $handle.Close(); $handle.Dispose() } catch {} }
        Remove-Item -LiteralPath $script:OdsStateLock -Force -ErrorAction SilentlyContinue
    }
}

function Set-OdsState {
    param([string]$Id, [ValidateSet('active','skip','undecided')] [string]$Status)
    Edit-OdsMachineState -Mutate {
        param($s)
        $s.active = @($s.active | Where-Object { $_ -ne $Id })
        $s.skip   = @($s.skip   | Where-Object { $_ -ne $Id })
        if ($Status -eq 'active') { $s.active = @($s.active) + $Id }
        elseif ($Status -eq 'skip') { $s.skip = @($s.skip) + $Id }
    }
}

function Move-OdsProjectState {
    <#
      Re-key a project's per-machine state when its id (Dest-relative path) changes.
      Moves active/skip membership and the deferred entry from $FromId to $ToId, and
      clears the old id's compare/maxDelete — the caller re-writes those under $ToId
      via Set-OdsProjectSettings. No-op when the id is unchanged.
    #>
    param([Parameter(Mandatory)][string]$FromId, [Parameter(Mandatory)][string]$ToId)
    if ($FromId -eq $ToId) { return }
    Edit-OdsMachineState -Mutate {
        param($s)
        if ($s.active -contains $FromId) { $s.active = @($s.active | Where-Object { $_ -ne $FromId }) + $ToId }
        if ($s.skip   -contains $FromId) { $s.skip   = @($s.skip   | Where-Object { $_ -ne $FromId }) + $ToId }
        if ($null -ne $s.deferred.PSObject.Properties[$FromId]) {
            $s.deferred | Add-Member -NotePropertyName $ToId -NotePropertyValue $s.deferred.$FromId -Force
            $s.deferred.PSObject.Properties.Remove($FromId)
        }
        $s.compare.PSObject.Properties.Remove($FromId)
        $s.maxDelete.PSObject.Properties.Remove($FromId)
    }
}

function Set-OdsProjectSettings {
    param([string]$Id, [string]$CompareMode, [object]$MaxDelete)
    Edit-OdsMachineState -Mutate {
        param($s)
        if ($CompareMode) {
            $s.compare | Add-Member -NotePropertyName $Id -NotePropertyValue $CompareMode -Force
        } else {
            $s.compare.PSObject.Properties.Remove($Id)
        }
        if ($null -ne $MaxDelete) {
            $s.maxDelete | Add-Member -NotePropertyName $Id -NotePropertyValue ([int]$MaxDelete) -Force
        } else {
            $s.maxDelete.PSObject.Properties.Remove($Id)
        }
    }
}

#endregion

# ============================================================================
#region  Path helpers + mirroring law
# ============================================================================

function Get-OdsRelUnder {
    <# Returns the path of $Full relative to $Root (using '\'), or $null if not under. #>
    param([string]$Full, [string]$Root)
    $f = [IO.Path]::GetFullPath($Full).TrimEnd('\')
    $r = [IO.Path]::GetFullPath($Root).TrimEnd('\')
    if ($f.Equals($r, 'OrdinalIgnoreCase')) { return '' }
    if ($f.StartsWith($r + '\', [StringComparison]::OrdinalIgnoreCase)) {
        return $f.Substring($r.Length + 1)
    }
    return $null
}
function Test-OdsOverlap {
    <# True if a is equal to / nested in b or vice-versa (E21/E22/E23). #>
    param([string]$A, [string]$B)
    $a = [IO.Path]::GetFullPath($A).TrimEnd('\'); $b = [IO.Path]::GetFullPath($B).TrimEnd('\')
    return ($a.Equals($b,'OrdinalIgnoreCase') -or
            $a.StartsWith($b + '\','OrdinalIgnoreCase') -or
            $b.StartsWith($a + '\','OrdinalIgnoreCase'))
}

function Test-OdsIsProtectedRoot {
    <#
      True if $Path is something we must never recursively delete or map a project
      onto: a drive root, the user profile, the OneDrive root, or an ANCESTOR of any
      of those. Fails closed — an unparseable/empty path is treated as protected.
    #>
    param([string]$Path)
    if ([string]::IsNullOrWhiteSpace($Path)) { return $true }
    $full = try { [IO.Path]::GetFullPath($Path).TrimEnd('\') } catch { return $true }
    if ([string]::IsNullOrWhiteSpace($full)) { return $true }
    if ([string]::IsNullOrEmpty([IO.Path]::GetDirectoryName($full))) { return $true }   # drive root
    $roots = @($env:USERPROFILE, $env:PUBLIC, $env:ProgramData, $env:windir)
    try { $roots += (Get-OdsOneDriveRoot) } catch {}
    foreach ($r in $roots) {
        if ([string]::IsNullOrWhiteSpace($r)) { continue }
        $rf = try { [IO.Path]::GetFullPath($r).TrimEnd('\') } catch { continue }
        if ($full.Equals($rf, [System.StringComparison]::OrdinalIgnoreCase)) { return $true }
        if ($rf.StartsWith($full + '\', [System.StringComparison]::OrdinalIgnoreCase)) { return $true }
    }
    return $false
}

#endregion

. (Join-Path $PSScriptRoot 'onedrive-sync-core.discovery.ps1')
. (Join-Path $PSScriptRoot 'onedrive-sync-core.engine.ps1')
