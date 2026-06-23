<#
.SYNOPSIS
    Headless check that every tray GUI event-handler resolves the commands it invokes.

.DESCRIPTION
    The tray crashed twice with "The term 'X' is not recognized" because a handler
    referenced a function that was out of scope at dispatch time (a window-nested
    function called from the timer's foreign scope). This guards that class: it loads
    the tray's functions via -NoStart, parses every Add_*/menu-item scriptblock, and
    verifies each invoked command resolves to a loaded function/cmdlet OR a nested
    function visible in the same window function. Any unresolved command is a latent
    "not recognized" crash and fails the run.

    Sibling to run-tests.ps1 (not part of it): the tray is WPF/STA and 5.1-only, so run
    this under  powershell.exe -STA  specifically.

.EXAMPLE
    powershell.exe -NoProfile -STA -ExecutionPolicy Bypass -File tests\verify-handlers.ps1
#>
param([string]$TrayPath = (Join-Path (Split-Path $PSScriptRoot -Parent) 'app-src\onedrive-sync-tray.ps1'))
$ErrorActionPreference = 'Stop'

# 1) Load the tray's functions (no tray start) so Get-Command knows them all.
. $TrayPath -NoStart

# 2) Parse the tray to an AST.
$errs = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($TrayPath, [ref]$null, [ref]$errs)
if ($errs) { $errs | ForEach-Object { Write-Host "PARSE: $($_.Message)" -ForegroundColor Red }; exit 2 }

# 3) For an AST node, the nested-function names visible in its scope = nested funcs of
#    every enclosing function on the chain (these resolve while that window is modal).
function Get-EnclosingContext($node) {
    $chain = @()
    $p = $node.Parent
    while ($p) {
        if ($p -is [System.Management.Automation.Language.FunctionDefinitionAst]) { $chain += $p }
        $p = $p.Parent
    }
    $visible = New-Object System.Collections.Generic.HashSet[string] ([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($f in $chain) {
        foreach ($nf in $f.Body.FindAll({ param($n) $n -is [System.Management.Automation.Language.FunctionDefinitionAst] }, $true)) {
            [void]$visible.Add($nf.Name)
        }
    }
    [pscustomobject]@{ Top = if ($chain.Count) { $chain[-1].Name } else { '<script>' }; Visible = $visible }
}

# 4) Every event-handler scriptblock: arguments to .Add_*(...) and ContextMenu .Items.Add(...).
$handlers = $ast.FindAll({
    param($n)
    $n -is [System.Management.Automation.Language.InvokeMemberExpressionAst] -and
    $n.Member -is [System.Management.Automation.Language.StringConstantExpressionAst] -and
    ($n.Member.Value -like 'Add_*' -or $n.Member.Value -eq 'Add')
}, $true)

# 5) Check each handler's invoked command names resolve in its scope.
$problems = @(); $checked = 0
foreach ($h in $handlers) {
    foreach ($sb in @($h.Arguments | Where-Object { $_ -is [System.Management.Automation.Language.ScriptBlockExpressionAst] })) {
        $checked++
        $ctx = Get-EnclosingContext $sb
        foreach ($c in $sb.FindAll({ param($n) $n -is [System.Management.Automation.Language.CommandAst] }, $true)) {
            $name = $c.GetCommandName()
            if (-not $name -or $name -match '^[\.\&]$') { continue }   # & $var / . $var — no static name
            if ($ctx.Visible.Contains($name)) { continue }
            if (Get-Command $name -ErrorAction Ignore) { continue }
            $problems += [pscustomobject]@{ In = $ctx.Top; Member = $h.Member.Value; Command = $name; Line = $c.Extent.StartLineNumber }
        }
    }
}

Write-Host "Handler scriptblocks checked: $checked"
if ($problems.Count) {
    Write-Host "UNRESOLVED COMMANDS (potential 'not recognized' crashes):" -ForegroundColor Red
    $problems | Sort-Object In, Command -Unique | Format-Table -AutoSize | Out-String | Write-Host
    exit 1
}
Write-Host "OK: every handler command resolves in its scope." -ForegroundColor Green
exit 0
