# OneDrive sync core — discovery (dot-sourced by onedrive-sync-core.ps1).
# Builds the available project set. A project object has:
#   id    : OneDrive-relative path (unique key)
#   kind  : 'mirror' | 'watch' | 'plain'
#   git   : [bool]  (false for plain folders -> skip git machinery)
#   name  : leaf name (for display)
#   local : absolute local path
#   dest  : absolute OneDrive path

function Test-OdsHasGit {
    param([string]$Dir)
    $g = Join-Path $Dir '.git'
    # .git can be a folder (normal repo) or a file (submodule/worktree).
    if (Test-Path -LiteralPath $g -PathType Container) { return 'dir' }
    if (Test-Path -LiteralPath $g -PathType Leaf)      { return 'file' }
    return $null
}

function Find-OdsGitRoots {
    <#
      Recursive, exclude-pruned walk of $Root. Yields a project root the moment a
      folder contains .git; does not descend past it (E11). Never enters $ExcludeDirs
      (so a dependency's .git inside node_modules is not misdetected).
      Returns objects: @{ Path; GitKind }.
    #>
    param([string]$Root, [string[]]$ExcludeDirs)

    if (-not (Test-Path -LiteralPath $Root -PathType Container)) { return @() }
    $excl = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($e in $ExcludeDirs) { [void]$excl.Add($e) }

    $results = [System.Collections.Generic.List[object]]::new()
    $stack = [System.Collections.Generic.Stack[string]]::new()
    $stack.Push($Root)
    while ($stack.Count -gt 0) {
        $dir = $stack.Pop()
        $kind = Test-OdsHasGit $dir
        if ($kind) {
            $results.Add([pscustomobject]@{ Path = $dir; GitKind = $kind })
            continue   # stop descending into a project root
        }
        try {
            $children = [IO.Directory]::GetDirectories($dir)
        } catch { continue }
        foreach ($c in $children) {
            $leaf = Split-Path -Leaf $c
            if ($excl.Contains($leaf)) { continue }
            $stack.Push($c)
        }
    }
    return $results
}

function New-OdsProject {
    param([string]$Id, [string]$Kind, [bool]$Git, [string]$Local, [string]$Dest)
    [pscustomobject]@{
        id    = $Id
        kind  = $Kind
        git   = $Git
        name  = Split-Path -Leaf $Dest
        local = $Local
        dest  = $Dest
    }
}

function Get-OdsProjects {
    <#
      Assemble the available project set (de-duplicated by id, minus tombstones).
      Sources: $ProjectParents (mirror, OneDrive side), catalog 'watch' entries,
      $PlainFolders (non-git), and locally-created repos under mirrored parents.
    #>
    param([hashtable]$Config)

    $od = Get-OdsOneDriveRoot
    $up = $env:USERPROFILE.TrimEnd('\')
    $catalog = Get-OdsCatalog
    $forgotten = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($f in @($catalog.forgotten)) { if ($f) { [void]$forgotten.Add($f) } }

    $byId = [ordered]@{}
    function _add($proj) {
        if (-not $proj) { return }
        if ($forgotten.Contains($proj.id)) { return }
        if (-not $byId.Contains($proj.id)) { $byId[$proj.id] = $proj }
    }

    # 1) Mirror projects: scan each OneDrive ProjectParent (OneDrive side) and the
    #    mirrored local parent (for locally-created repos not yet on OneDrive).
    foreach ($parent in @($Config.ProjectParents)) {
        $rel = Get-OdsRelUnder -Full $parent -Root $od
        if ($null -eq $rel) { Write-OdsLog "ProjectParent '$parent' is not under OneDrive root; skipped." 'WARN'; continue }
        $localParent = if ($rel) { Join-Path $up $rel } else { $up }

        foreach ($side in @($parent, $localParent)) {
            foreach ($hit in (Find-OdsGitRoots -Root $side -ExcludeDirs $Config.ExcludeDirs)) {
                if ($hit.GitKind -eq 'file') {
                    Write-OdsLog "Skipping .git-file project (submodule/worktree): $($hit.Path)" 'WARN'
                    continue
                }
                # compute id (relative to OneDrive root) for either side
                $r = Get-OdsRelUnder -Full $hit.Path -Root $od
                if ($null -eq $r) { $r = Get-OdsRelUnder -Full $hit.Path -Root $up }
                if ($null -eq $r) { continue }
                $dest  = Join-Path $od $r
                $local = Join-Path $up $r
                _add (New-OdsProject -Id $r -Kind 'mirror' -Git $true -Local $local -Dest $dest)
            }
        }
    }

    # 2) Watch-root projects (arbitrary dest) — known ones come from the catalog;
    #    brand-new ones are surfaced by Find-OdsNewWatchProjects at decision time.
    foreach ($e in @($catalog.entries)) {
        if ($e.kind -eq 'watch') {
            $local = Join-Path $up $e.localRel
            $dest  = Join-Path $od $e.destRel
            _add (New-OdsProject -Id $e.id -Kind 'watch' -Git $true -Local $local -Dest $dest)
        }
    }

    # 3) Plain (non-git) folders.
    foreach ($pf in @($Config.PlainFolders)) {
        if (-not $pf.Dest -or -not $pf.Local) { continue }
        $r = Get-OdsRelUnder -Full $pf.Dest -Root $od
        $id = if ($r) { $r } else { $pf.Dest }
        _add (New-OdsProject -Id $id -Kind 'plain' -Git $false -Local $pf.Local -Dest $pf.Dest)
    }

    return @($byId.Values)
}

function Find-OdsNewWatchProjects {
    <#
      Git repos found under $WatchRoots that have no catalog entry and whose
      (id) path doesn't already correspond to a mirror project. These need a
      destination chosen (folder-picker / pending).
    #>
    param([hashtable]$Config, [object[]]$Known)
    $up = $env:USERPROFILE.TrimEnd('\')
    $knownLocal = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($k in $Known) { [void]$knownLocal.Add(([IO.Path]::GetFullPath($k.local)).TrimEnd('\')) }

    $new = [System.Collections.Generic.List[object]]::new()
    foreach ($root in @($Config.WatchRoots)) {
        foreach ($hit in (Find-OdsGitRoots -Root $root -ExcludeDirs $Config.ExcludeDirs)) {
            if ($hit.GitKind -ne 'dir') { continue }
            $full = ([IO.Path]::GetFullPath($hit.Path)).TrimEnd('\')
            if ($knownLocal.Contains($full)) { continue }
            $rel = Get-OdsRelUnder -Full $full -Root $up
            $new.Add([pscustomobject]@{ Local = $full; SuggestRel = $rel; Name = Split-Path -Leaf $full })
        }
    }
    return $new
}
