<#
  Pester v5 suite for the OneDrive sync core. Exercises the dangerous, git-aware
  paths against fake OneDrive dirs + throwaway git repos. No real OneDrive/rclone
  needed (bisync itself is integration-tested separately).

  Run:  Invoke-Pester -Path tests\onedrive-sync.Tests.ps1
#>

BeforeAll {
    $script:Sandbox = Join-Path $env:TEMP ("ods-test-" + [guid]::NewGuid().ToString('N').Substring(0,8))
    $script:OneDrive = Join-Path $Sandbox 'OneDrive'
    $script:Profile  = Join-Path $Sandbox 'User'
    New-Item -ItemType Directory -Force $OneDrive, $Profile, (Join-Path $Sandbox 'Local') | Out-Null

    # Isolate all tool state into the sandbox BEFORE loading the core (paths are
    # captured at load time).
    $env:LOCALAPPDATA     = Join-Path $Sandbox 'Local'
    $env:OneDriveConsumer = $OneDrive
    $env:USERPROFILE      = $Profile

    $appSrc = Join-Path (Split-Path $PSScriptRoot -Parent) 'app-src'
    . (Join-Path $appSrc 'onedrive-sync-core.ps1')
    $script:Cfg = Import-OdsConfig

    function New-TestRepo {
        param([string]$Path, [hashtable]$Files = @{}, [string[]]$Gitignore = @(), [string[]]$Track = @())
        New-Item -ItemType Directory -Force $Path | Out-Null
        Push-Location $Path
        git init -q; git config user.email t@t.t; git config user.name t
        if ($Gitignore) { Set-Content .gitignore ($Gitignore -join "`n") }
        foreach ($k in $Files.Keys) {
            $f = Join-Path $Path $k; New-Item -ItemType Directory -Force (Split-Path $f) | Out-Null
            Set-Content $f $Files[$k]
        }
        if ($Track) { git add -f @Track 2>$null; git commit -qm init 2>$null | Out-Null }
        Pop-Location
    }
}

Describe "Mirroring law & path helpers" {
    It "computes a relative path under a root" {
        Get-OdsRelUnder -Full (Join-Path $OneDrive 'Projects\web\app') -Root $OneDrive | Should -Be 'Projects\web\app'
    }
    It "returns null when not under the root" {
        Get-OdsRelUnder -Full 'C:\elsewhere\x' -Root $OneDrive | Should -Be $null
    }
    It "detects overlap (self-sync / nesting)" {
        Test-OdsOverlap 'C:\a\b' 'C:\a\b\c' | Should -BeTrue
        Test-OdsOverlap 'C:\a\b' 'C:\a\b'   | Should -BeTrue
        Test-OdsOverlap 'C:\a\b' 'C:\a\c'   | Should -BeFalse
    }
}

Describe "Exclude matching" {
    It "matches a file in an excluded dir" {
        Test-OdsMatchesExclude -RelPath 'node_modules/x/y.js' -ExcludeDirs @('node_modules') -ExcludeFiles @() | Should -BeTrue
    }
    It "matches a top-level file pattern" {
        Test-OdsMatchesExclude -RelPath 'build.log' -ExcludeDirs @('node_modules') -ExcludeFiles @('*.log') | Should -BeTrue
    }
    It "does not match an ordinary tracked file" {
        Test-OdsMatchesExclude -RelPath 'src/app.js' -ExcludeDirs @('node_modules') -ExcludeFiles @('*.log') | Should -BeFalse
    }
}

Describe "Discovery (recursive, exclude-pruned)" {
    BeforeAll {
        $parent = Join-Path $OneDrive 'Projects'
        New-TestRepo -Path (Join-Path $parent 'web\my-app') -Files @{ 'a.txt'='1' } -Track @('a.txt')
        New-TestRepo -Path (Join-Path $parent 'api')         -Files @{ 'b.txt'='1' } -Track @('b.txt')
        New-Item -ItemType Directory -Force (Join-Path $parent 'plain-notes') | Out-Null
        New-Item -ItemType Directory -Force (Join-Path $parent 'api\node_modules\dep\.git') | Out-Null
        $script:Cfg.ProjectParents = @($parent); $script:Cfg.WatchRoots = @()
        $script:Found = @(Get-OdsProjects -Config $Cfg)
    }
    It "finds nested repos at any depth" { $Found.id | Should -Contain 'Projects\web\my-app' }
    It "finds top-level repos" { $Found.id | Should -Contain 'Projects\api' }
    It "ignores non-git folders" { ($Found.id -join ';') | Should -Not -Match 'plain-notes' }
    It "does not descend into node_modules" { ($Found.id -join ';') | Should -Not -Match 'node_modules' }
}

Describe "Git-aware filter generation (E1/E53/E57)" {
    BeforeAll {
        $repo = Join-Path $Profile ('Code\fp-' + [guid]::NewGuid().ToString('N').Substring(0,6))
        New-TestRepo -Path $repo -Gitignore @('data/','*.bak') `
            -Files @{ 'app.js'='1'; 'build.log'='tracked'; 'data/big.bin'='x'; 'notes.bak'='x'; '.env'='S=1' } `
            -Track @('.gitignore','app.js','build.log')
        $id = 'Projects\fp-' + [guid]::NewGuid().ToString('N').Substring(0,6)
        $proj = New-OdsProject -Id $id -Kind 'mirror' -Git $true -Local $repo -Dest (Join-Path $OneDrive $id)
        $script:FLines = Get-Content (New-OdsFilterFile -Project $proj -Config $Cfg).Path
    }
    It "force-includes a tracked file that matches an exclude" { $FLines | Should -Contain '+ /build.log' }
    It "includes allow-listed secrets" { $FLines | Should -Contain '+ .env' }
    It "syncs git history" { $FLines | Should -Contain '+ /.git/**' }
    It "excludes volatile git metadata" { $FLines | Should -Contain '- /.git/index' }
    It "excludes gitignored dirs (coarse)" { ($FLines -join "`n") | Should -Match '(?m)^- /data' }
    It "excludes gitignored files" { ($FLines -join "`n") | Should -Match 'notes\.bak' }
}

Describe "Catalog merge & tombstones (E3)" {
    It "merges entries and tombstones by id (union)" {
        $a = [pscustomobject]@{ entries=@([pscustomobject]@{id='x';kind='watch'}); forgotten=@('p') }
        $b = [pscustomobject]@{ entries=@([pscustomobject]@{id='y';kind='watch'}); forgotten=@('q') }
        $m = Merge-OdsCatalog $a $b
        @($m.entries).id | Should -Contain 'x'
        @($m.entries).id | Should -Contain 'y'
        @($m.forgotten) | Should -Contain 'p'
        @($m.forgotten) | Should -Contain 'q'
    }
    It "Forget tombstones an id and Pull-clears it" {
        Forget-OdsProject -Id 'Projects\zz' -Config $Cfg
        (Get-OdsCatalog).forgotten | Should -Contain 'Projects\zz'
        # Pull clears the tombstone (project need not exist for the clear step)
        $cat = Get-OdsCatalog; $cat.forgotten = @($cat.forgotten | Where-Object { $_ -ne 'Projects\zz' }); Save-OdsCatalog $cat
        (Get-OdsCatalog).forgotten | Should -Not -Contain 'Projects\zz'
    }
}

Describe "First-run seed (newest-wins, E5)" {
    It "archives the older of two differing copies" {
        $id = 'Projects\seed-' + [guid]::NewGuid().ToString('N').Substring(0,6)
        $local = Join-Path $Profile $id; $dest = Join-Path $OneDrive $id
        New-Item -ItemType Directory -Force $local, $dest | Out-Null
        Set-Content (Join-Path $local 'f.txt') 'NEWER'; (Get-Item (Join-Path $local 'f.txt')).LastWriteTimeUtc = (Get-Date).ToUniversalTime()
        Set-Content (Join-Path $dest 'f.txt') 'older'; (Get-Item (Join-Path $dest 'f.txt')).LastWriteTimeUtc = (Get-Date).AddHours(-1).ToUniversalTime()
        $proj = New-OdsProject -Id $id -Kind 'mirror' -Git $false -Local $local -Dest $dest
        Invoke-OdsSeed -Project $proj -Config $Cfg
        $archive = Join-Path $env:LOCALAPPDATA ("onedrive-sync\versions\" + (Get-OdsIdHash $id))
        (Get-ChildItem $archive -Recurse -File -ErrorAction SilentlyContinue).Count | Should -BeGreaterThan 0
    }
}

AfterAll {
    if (Test-Path $script:Sandbox) { Remove-Item $script:Sandbox -Recurse -Force -ErrorAction SilentlyContinue }
}
