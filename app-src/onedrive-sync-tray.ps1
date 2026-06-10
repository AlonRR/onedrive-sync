<#
.SYNOPSIS
    OneDrive 2-way sync — system-tray helper + GUI management window.

.DESCRIPTION
    Runs at logon as a NotifyIcon. Reflects sync state, watches pending.json to
    surface newly-available projects, and opens a management window (also via
    -ShowWindow / onedrive-sync.ps1 -Gui). All actions call the shared core, so
    they behave identically to the CLI.

.PARAMETER ShowWindow  Open the management window immediately (instead of just the tray).
#>
param([switch]$ShowWindow)

# WinForms requires STA — relaunch under Windows PowerShell STA if needed.
if ([System.Threading.Thread]::CurrentThread.GetApartmentState() -ne 'STA') {
    $argsList = @('-NoProfile','-STA','-ExecutionPolicy','Bypass','-File', "`"$($MyInvocation.MyCommand.Path)`"")
    if ($ShowWindow) { $argsList += '-ShowWindow' }
    Start-Process powershell.exe -ArgumentList $argsList -WindowStyle Hidden
    return
}

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
. (Join-Path $PSScriptRoot 'onedrive-sync-core.ps1')

$script:Cfg = $null
function Get-Cfg { if (-not $script:Cfg) { $script:Cfg = Import-OdsConfig }; return $script:Cfg }

$AppPwsh = Get-Command pwsh -ErrorAction SilentlyContinue
if (-not $AppPwsh) { $AppPwsh = Get-Command powershell }
$CliPath = Join-Path $PSScriptRoot 'onedrive-sync.ps1'
function Invoke-Cli { param([string[]]$CliArgs)
    Start-Process $AppPwsh.Source -ArgumentList (@('-NoProfile','-ExecutionPolicy','Bypass','-File',"`"$CliPath`"",'-NoUpdate') + $CliArgs) -WindowStyle Hidden
}

# --------------------------------------------------------------------------
#  Management window
# --------------------------------------------------------------------------
function Show-OdsWindow {
    $cfg = Get-Cfg
    $form = New-Object System.Windows.Forms.Form
    $form.Text = 'OneDrive Sync — Projects'
    $form.Size = New-Object System.Drawing.Size(900, 520)
    $form.StartPosition = 'CenterScreen'

    $grid = New-Object System.Windows.Forms.DataGridView
    $grid.Dock = 'Fill'; $grid.ReadOnly = $true; $grid.AllowUserToAddRows = $false
    $grid.SelectionMode = 'FullRowSelect'; $grid.AutoSizeColumnsMode = 'Fill'
    $grid.MultiSelect = $true

    $panel = New-Object System.Windows.Forms.FlowLayoutPanel
    $panel.Dock = 'Bottom'; $panel.Height = 40; $panel.FlowDirection = 'LeftToRight'

    function Refresh-Grid {
        $rows = @(Get-OdsProjectStatus -Config $cfg | Select-Object Status, Kind,
                  @{N='Git';E={if($_.Git){'git'}else{'plain'}}},
                  @{N='Local';E={if($_.LocalPresent){'yes'}else{'-'}}}, Conflicts, Id)
        $grid.DataSource = [System.Collections.ArrayList]@($rows)
    }
    function Selected-Ids { @($grid.SelectedRows | ForEach-Object { $_.Cells['Id'].Value }) }

    function Add-Button($text, $action) {
        $b = New-Object System.Windows.Forms.Button
        $b.Text = $text; $b.AutoSize = $true; $b.Add_Click($action); $panel.Controls.Add($b) | Out-Null
    }
    Add-Button 'Sync now'      { foreach ($id in Selected-Ids) { Invoke-Cli @('-SyncNow', $id) }; [System.Windows.Forms.MessageBox]::Show('Sync started.') }
    Add-Button 'Pull here'     { foreach ($id in Selected-Ids) { Invoke-Cli @('-Pull', $id) }; Start-Sleep 1; Refresh-Grid }
    Add-Button 'Unmap'         { foreach ($id in Selected-Ids) { Invoke-Cli @('-Unmap', $id) }; Start-Sleep 1; Refresh-Grid }
    Add-Button 'Forget'        { foreach ($id in Selected-Ids) { Invoke-Cli @('-Forget', $id) }; Start-Sleep 1; Refresh-Grid }
    Add-Button 'Open local'    {
        foreach ($id in Selected-Ids) {
            $p = @(Get-OdsProjects -Config $cfg) | Where-Object id -eq $id | Select-Object -First 1
            if ($p -and (Test-Path $p.local)) { Start-Process explorer.exe $p.local }
        }
    }
    Add-Button 'Conflicts'     { Invoke-Cli @('-Conflicts') }
    Add-Button 'Discover new'  { Show-OdsPicker; Refresh-Grid }
    Add-Button 'Show retired'  { Show-OdsRetired; Refresh-Grid }
    Add-Button 'Refresh'       { Refresh-Grid }

    $form.Controls.Add($grid); $form.Controls.Add($panel)
    Refresh-Grid
    [void]$form.ShowDialog()
}

# Multi-select picker for undecided projects (pull).
function Show-OdsPicker {
    $cfg = Get-Cfg
    $state = Get-OdsMachineState
    $known = @(Get-OdsProjects -Config $cfg)
    $undecided = @($known | Where-Object { $state.active -notcontains $_.id -and $state.skip -notcontains $_.id -and -not (Test-Path -LiteralPath $_.local) })
    if (-not $undecided) { [System.Windows.Forms.MessageBox]::Show('No new projects to choose.'); return }

    $f = New-Object System.Windows.Forms.Form
    $f.Text = 'Choose projects to sync here'; $f.Size = New-Object System.Drawing.Size(600,420); $f.StartPosition='CenterScreen'
    $clb = New-Object System.Windows.Forms.CheckedListBox; $clb.Dock='Fill'
    foreach ($u in $undecided) { [void]$clb.Items.Add($u.id) }
    $ok = New-Object System.Windows.Forms.Button; $ok.Text='Sync selected'; $ok.Dock='Bottom'; $ok.DialogResult='OK'
    $f.Controls.Add($clb); $f.Controls.Add($ok); $f.AcceptButton=$ok
    if ($f.ShowDialog() -eq 'OK') {
        for ($i=0; $i -lt $clb.Items.Count; $i++) {
            $id = $clb.Items[$i]
            if ($clb.GetItemChecked($i)) { Invoke-Cli @('-Pull', $id) }
            else { Set-OdsState -Id $id -Status skip }
        }
    }
}

function Show-OdsRetired {
    $cat = Get-OdsCatalog
    if (-not @($cat.forgotten)) { [System.Windows.Forms.MessageBox]::Show('No retired projects.'); return }
    $f = New-Object System.Windows.Forms.Form
    $f.Text='Retired projects (un-forget)'; $f.Size=New-Object System.Drawing.Size(560,360); $f.StartPosition='CenterScreen'
    $clb = New-Object System.Windows.Forms.CheckedListBox; $clb.Dock='Fill'
    foreach ($id in @($cat.forgotten)) { [void]$clb.Items.Add($id) }
    $ok = New-Object System.Windows.Forms.Button; $ok.Text='Revive selected'; $ok.Dock='Bottom'; $ok.DialogResult='OK'
    $f.Controls.Add($clb); $f.Controls.Add($ok)
    if ($f.ShowDialog() -eq 'OK') {
        for ($i=0; $i -lt $clb.Items.Count; $i++) { if ($clb.GetItemChecked($i)) { Invoke-Cli @('-Pull', $clb.Items[$i]) } }
    }
}

# --------------------------------------------------------------------------
#  Tray icon
# --------------------------------------------------------------------------
$icon = New-Object System.Windows.Forms.NotifyIcon
$icon.Icon = [System.Drawing.SystemIcons]::Application
$icon.Text = 'OneDrive Sync'
$icon.Visible = $true

$menu = New-Object System.Windows.Forms.ContextMenuStrip
[void]$menu.Items.Add('Sync all now',  $null, { Invoke-Cli @('-SyncNow','*') })
[void]$menu.Items.Add('Manage…',       $null, { Show-OdsWindow })
[void]$menu.Items.Add('Choose new projects…', $null, { Show-OdsPicker })
[void]$menu.Items.Add('-')
[void]$menu.Items.Add('Pause sync',    $null, { Invoke-Cli @('-Pause') })
[void]$menu.Items.Add('Resume sync',   $null, { Invoke-Cli @('-Resume') })
[void]$menu.Items.Add('Open log',      $null, { Start-Process notepad.exe (Join-Path $env:LOCALAPPDATA 'onedrive-sync\logs\sync.log') })
[void]$menu.Items.Add('-')
[void]$menu.Items.Add('Exit',          $null, { $icon.Visible=$false; [System.Windows.Forms.Application]::Exit() })
$icon.ContextMenuStrip = $menu
$icon.Add_MouseClick({ if ($_.Button -eq 'Left') { Show-OdsWindow } })

# Poll pending.json and reflect count in the tray.
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 15000
$script:LastPending = -1
$timer.Add_Tick({
    try {
        $pendingFile = Join-Path $env:LOCALAPPDATA 'onedrive-sync\pending.json'
        $n = 0
        if (Test-Path $pendingFile) { $n = @(Get-Content $pendingFile -Raw | ConvertFrom-Json).Count }
        if ($n -ne $script:LastPending) {
            $icon.Text = if ($n -gt 0) { "OneDrive Sync — $n project(s) available" } else { 'OneDrive Sync' }
            if ($n -gt 0 -and $script:LastPending -ge 0) {
                $icon.BalloonTipTitle = 'OneDrive Sync'
                $icon.BalloonTipText  = "$n new project(s) available. Click to choose."
                $icon.ShowBalloonTip(5000)
            }
            $script:LastPending = $n
        }
    } catch { }
})
$timer.Start()
$icon.Add_BalloonTipClicked({ Show-OdsPicker })

if ($ShowWindow) { Show-OdsWindow }
[System.Windows.Forms.Application]::Run()
$icon.Visible = $false
