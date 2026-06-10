<#
.SYNOPSIS
    Dependency-free test runner for the OneDrive sync core (no Pester required).

.DESCRIPTION
    Mirrors tests\onedrive-sync.Tests.ps1 for environments without Pester v5.
    Exercises the dangerous, git-aware paths against fake OneDrive dirs + temp
    git repos, then prints a PASS/FAIL summary and exits non-zero on failure.
#>
$ErrorActionPreference = 'Stop'
$script:Pass = 0; $script:Fail = 0
function Check($name, [bool]$cond) {
    if ($cond) { Write-Host "  PASS  $name" -ForegroundColor Green; $script:Pass++ }
    else       { Write-Host "  FAIL  $name" -ForegroundColor Red;   $script:Fail++ }
}

# --- isolated sandbox env (set BEFORE loading the core) ---
$Sandbox  = Join-Path $env:TEMP ("ods-rt-" + [guid]::NewGuid().ToString('N').Substring(0,8))
$OneDrive = Join-Path $Sandbox 'OneDrive'
$Profile2 = Join-Path $Sandbox 'User'
New-Item -ItemType Directory -Force $OneDrive, $Profile2, (Join-Path $Sandbox 'Local') | Out-Null
$env:LOCALAPPDATA     = Join-Path $Sandbox 'Local'
$env:OneDriveConsumer = $OneDrive
$env:USERPROFILE      = $Profile2

. (Join-Path (Split-Path $PSScriptRoot -Parent) 'app-src\onedrive-sync-core.ps1')
$cfg = Import-OdsConfig

function New-TestRepo {
    param([string]$Path, [hashtable]$Files=@{}, [string[]]$Gitignore=@(), [string[]]$Track=@())
    New-Item -ItemType Directory -Force $Path | Out-Null
    Push-Location $Path
    git init -q; git config user.email t@t.t; git config user.name t
    if ($Gitignore) { Set-Content .gitignore ($Gitignore -join "`n") }
    foreach ($k in $Files.Keys) { $f = Join-Path $Path $k; New-Item -ItemType Directory -Force (Split-Path $f) | Out-Null; Set-Content $f $Files[$k] }
    if ($Track) { git add -f @Track 2>$null; git commit -qm init 2>$null | Out-Null }
    Pop-Location
}

try {
    Write-Host "Path helpers / mirroring law"
    Check "rel under root" ((Get-OdsRelUnder -Full (Join-Path $OneDrive 'Projects\web\app') -Root $OneDrive) -eq 'Projects\web\app')
    Check "rel null when outside" ($null -eq (Get-OdsRelUnder -Full 'C:\elsewhere\x' -Root $OneDrive))
    Check "overlap nested" (Test-OdsOverlap 'C:\a\b' 'C:\a\b\c')
    Check "overlap equal"  (Test-OdsOverlap 'C:\a\b' 'C:\a\b')
    Check "no overlap"     (-not (Test-OdsOverlap 'C:\a\b' 'C:\a\c'))

    Write-Host "Exclude matching"
    Check "file in excluded dir" (Test-OdsMatchesExclude -RelPath 'node_modules/x/y.js' -ExcludeDirs @('node_modules') -ExcludeFiles @())
    Check "top-level pattern"    (Test-OdsMatchesExclude -RelPath 'build.log' -ExcludeDirs @() -ExcludeFiles @('*.log'))
    Check "ordinary file"        (-not (Test-OdsMatchesExclude -RelPath 'src/app.js' -ExcludeDirs @('node_modules') -ExcludeFiles @('*.log')))

    Write-Host "Discovery (recursive, pruned)"
    $parent = Join-Path $OneDrive 'Projects'
    New-TestRepo -Path (Join-Path $parent 'web\my-app') -Files @{'a.txt'='1'} -Track @('a.txt')
    New-TestRepo -Path (Join-Path $parent 'api')         -Files @{'b.txt'='1'} -Track @('b.txt')
    New-Item -ItemType Directory -Force (Join-Path $parent 'plain-notes') | Out-Null
    New-Item -ItemType Directory -Force (Join-Path $parent 'api\node_modules\dep\.git') | Out-Null
    $cfg.ProjectParents = @($parent); $cfg.WatchRoots = @()
    $found = @(Get-OdsProjects -Config $cfg)
    Check "finds nested repo" ($found.id -contains 'Projects\web\my-app')
    Check "finds top repo"    ($found.id -contains 'Projects\api')
    Check "ignores non-git"   (-not (($found.id -join ';') -match 'plain-notes'))
    Check "prunes node_modules" (-not (($found.id -join ';') -match 'node_modules'))

    Write-Host "Git-aware filter"
    $repo = Join-Path $Profile2 'Code\fp'
    New-TestRepo -Path $repo -Gitignore @('data/','*.bak') `
        -Files @{'app.js'='1';'build.log'='t';'data/big.bin'='x';'notes.bak'='x';'.env'='S=1'} `
        -Track @('.gitignore','app.js','build.log')
    $proj = New-OdsProject -Id 'Projects\fp' -Kind 'mirror' -Git $true -Local $repo -Dest (Join-Path $OneDrive 'Projects\fp')
    $fl = Get-Content (New-OdsFilterFile -Project $proj -Config $cfg).Path
    Check "tracked exception"   ($fl -contains '+ /build.log')
    Check "allow-list .env"     ($fl -contains '+ .env')
    Check "git history"         ($fl -contains '+ /.git/**')
    Check "volatile git excluded" ($fl -contains '- /.git/index')
    Check "gitignore data/ excluded" ([bool](($fl -join "`n") -match '(?m)^- /data'))
    Check "gitignore *.bak excluded" ([bool](($fl -join "`n") -match 'notes\.bak'))

    Write-Host "Catalog merge & tombstones"
    $a = [pscustomobject]@{ entries=@([pscustomobject]@{id='x';kind='watch'}); forgotten=@('p') }
    $b = [pscustomobject]@{ entries=@([pscustomobject]@{id='y';kind='watch'}); forgotten=@('q') }
    $m = Merge-OdsCatalog $a $b
    Check "merge entries" (($m.entries.id -contains 'x') -and ($m.entries.id -contains 'y'))
    Check "merge tombstones" ((@($m.forgotten) -contains 'p') -and (@($m.forgotten) -contains 'q'))
    Forget-OdsProject -Id 'Projects\zz' -Config $cfg
    Check "forget tombstones" ((Get-OdsCatalog).forgotten -contains 'Projects\zz')

    Write-Host "First-run seed (newest-wins)"
    $sid = 'Projects\seed'; $sl = Join-Path $Profile2 $sid; $sd = Join-Path $OneDrive $sid
    New-Item -ItemType Directory -Force $sl, $sd | Out-Null
    Set-Content (Join-Path $sl 'f.txt') 'NEWER'; (Get-Item (Join-Path $sl 'f.txt')).LastWriteTimeUtc = (Get-Date).ToUniversalTime()
    Set-Content (Join-Path $sd 'f.txt') 'older'; (Get-Item (Join-Path $sd 'f.txt')).LastWriteTimeUtc = (Get-Date).AddHours(-1).ToUniversalTime()
    Invoke-OdsSeed -Project (New-OdsProject -Id $sid -Kind 'mirror' -Git $false -Local $sl -Dest $sd) -Config $cfg
    $arch = Join-Path $env:LOCALAPPDATA ("onedrive-sync\versions\" + (Get-OdsIdHash $sid))
    Check "seed archived loser" (@(Get-ChildItem $arch -Recurse -File -ErrorAction SilentlyContinue).Count -gt 0)
}
finally {
    if (Test-Path $Sandbox) { Remove-Item $Sandbox -Recurse -Force -ErrorAction SilentlyContinue }
}

Write-Host ""
$color = if ($script:Fail) { 'Red' } else { 'Green' }
Write-Host ("TOTAL: {0} passed, {1} failed" -f $script:Pass, $script:Fail) -ForegroundColor $color
if ($script:Fail) { exit 1 }
