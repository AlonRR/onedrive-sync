<#
.SYNOPSIS
    OneDrive 2-way sync - system-tray helper + WPF management window.

.DESCRIPTION
    Runs at logon as a NotifyIcon. Reflects sync state, watches pending.json to
    surface newly-available projects, and opens a WPF management window (also via
    -ShowWindow / onedrive-sync.ps1 -Gui). All actions call the shared core.

.PARAMETER ShowWindow  Open the management window immediately.
#>
param([switch]$ShowWindow)

# WPF requires STA - relaunch under Windows PowerShell STA if needed.
if ([System.Threading.Thread]::CurrentThread.GetApartmentState() -ne 'STA') {
    $argsList = @('-NoProfile', '-STA', '-ExecutionPolicy', 'Bypass', '-File', "`"$($MyInvocation.MyCommand.Path)`"")
    if ($ShowWindow) { $argsList += '-ShowWindow' }
    Start-Process powershell.exe -ArgumentList $argsList -WindowStyle Hidden
    return
}

Add-Type -AssemblyName PresentationFramework
Add-Type -AssemblyName PresentationCore
Add-Type -AssemblyName WindowsBase
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class Win32Icon {
    [DllImport("user32.dll", CharSet = CharSet.Auto)]
    public static extern bool DestroyIcon(IntPtr handle);
}
'@ -ErrorAction SilentlyContinue

. (Join-Path $PSScriptRoot 'onedrive-sync-core.ps1')

# Ensure a WPF Application context exists (needed for WPF windows in a WinForms host).
if (-not [System.Windows.Application]::Current) { $null = New-Object System.Windows.Application }

$script:Cfg            = $null
$script:CachedRows     = $null   # [object[]] populated by background runspace; $null = not ready yet
$script:RefreshHandle  = $null   # { PS, Async, RS } active background status job
$script:LastRunEndTs   = $null   # ISO timestamp of last run-end event seen; '' = none today; null = not yet checked
$script:SyncStartedAt  = $null   # [datetime] when a manual sync was started; $null = idle
$script:IconIsOwned    = $false  # $true when current tray icon was drawn by New-StatusIcon (has owned HICON)
$script:LastIconColor  = $null   # last color passed to New-StatusIcon; skip GDI alloc on no-change
$script:StartedAt      = [datetime]::Now  # startup time for stale-grace-period on fresh install
function Get-Cfg { if (-not $script:Cfg) { $script:Cfg = Import-OdsConfig }; return $script:Cfg }

function Test-OdsSyncRunning {
    # Read-only lock check - does NOT acquire the lock, so it cannot block a sync subprocess.
    if (-not (Test-Path -LiteralPath $script:OdsLockFile)) { return $false }
    try {
        $lock = Get-Content -LiteralPath $script:OdsLockFile -Raw | ConvertFrom-Json
        if (-not $lock.pid) { return $false }
        $alive = [bool](Get-Process -Id $lock.pid -ErrorAction SilentlyContinue)
        if (-not $alive) { return $false }
        $age = (Get-Date) - [datetime]$lock.ts
        return $age.TotalMinutes -lt 60
    } catch { return $false }
}

$AppPwsh = Get-Command pwsh -ErrorAction SilentlyContinue
if (-not $AppPwsh) { $AppPwsh = Get-Command powershell }
$CliPath = Join-Path $PSScriptRoot 'onedrive-sync.ps1'

function Invoke-Cli {
    param([string[]]$CliArgs)
    Start-Process $AppPwsh.Source -ArgumentList (@('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$CliPath`"", '-NoUpdate') + $CliArgs) -WindowStyle Hidden
}

function New-WpfWindow {
    param([string]$Xaml)
    $reader = [System.Xml.XmlReader]::Create([System.IO.StringReader]::new($Xaml))
    return [System.Windows.Markup.XamlReader]::Load($reader)
}

function Get-StatusBrush {
    param($status, $conflicts)
    if ($conflicts -gt 0)  { return [System.Windows.Media.Brushes]::OrangeRed }
    switch ($status) {
        'active' { return [System.Windows.Media.Brushes]::ForestGreen }
        'skip'   { return [System.Windows.Media.Brushes]::LightGray }
        default  { return [System.Windows.Media.Brushes]::SteelBlue }
    }
}

function Get-OdsLastRunEndEvent {
    try {
        $candidates = @(
            [datetime]::UtcNow
            [datetime]::UtcNow.AddDays(-1)
        ) | ForEach-Object {
            $evf = Join-Path $env:LOCALAPPDATA ("onedrive-sync\events\{0}.jsonl" -f $_.ToString('yyyy-MM-dd'))
            if (Test-Path $evf) {
                Get-Content $evf -Tail 200 -ErrorAction SilentlyContinue | ForEach-Object {
                    try { $_ | ConvertFrom-Json } catch {}
                } | Where-Object { $_ -and $_.event -eq 'run-end' }
            }
        }
        return @($candidates) | Select-Object -Last 1
    } catch { return $null }
}

function Get-LastSyncText {
    try {
        $last = Get-OdsLastRunEndEvent
        if (-not $last) { return 'No sync today' }
        $ago = [datetime]::Now - [datetime]::Parse($last.ts).ToLocalTime()
        if ($ago.TotalMinutes -lt 1)  { return 'Last sync: just now' }
        if ($ago.TotalMinutes -lt 60) { return "Last sync: $([int]$ago.TotalMinutes) min ago" }
        if ($ago.TotalHours   -lt 24) { return "Last sync: $([int]$ago.TotalHours)h ago" }
        return "Last sync: $([datetime]::Parse($last.ts).ToLocalTime().ToString('MMM d, h:mm tt'))"
    } catch { return '' }
}

function New-StatusIcon {
    param([string]$Color = 'DimGray')
    try {
        $bmp   = New-Object System.Drawing.Bitmap 16, 16
        $g     = [System.Drawing.Graphics]::FromImage($bmp)
        $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
        $brush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromName($Color))
        $g.FillEllipse($brush, 1, 1, 13, 13)
        $g.Dispose(); $brush.Dispose()
        $handle = $bmp.GetHicon()
        $ico    = [System.Drawing.Icon]::FromHandle($handle)
        $bmp.Dispose()
        $script:IconIsOwned = $true
        return $ico
    } catch {
        $script:IconIsOwned = $false
        return [System.Drawing.SystemIcons]::Application
    }
}

function Update-TrayIcon {
    $rows = $script:CachedRows
    if ($null -eq $rows) { return }
    $conflicts = [int](($rows | Measure-Object -Property Conflicts -Sum).Sum)
    $active    = @($rows | Where-Object { $_.Status -eq 'active' }).Count

    # Icon color priority: conflicts > syncing > stale > ok > no-projects
    $color = if ($conflicts -gt 0) {
        'Crimson'
    } elseif ($null -ne $script:SyncStartedAt) {
        'SteelBlue'
    } elseif ($active -eq 0) {
        'DimGray'
    } else {
        $stale = if ($null -eq $script:LastRunEndTs) {
            $false   # not yet initialized - don't show stale prematurely
        } elseif ($script:LastRunEndTs -eq '') {
            ([datetime]::Now - $script:StartedAt).TotalHours -gt 2  # grace period on fresh install / first run of day
        } else {
            try { ([datetime]::Now - [datetime]::Parse($script:LastRunEndTs).ToLocalTime()).TotalHours -gt 2 } catch { $false }
        }
        if ($stale) { 'Goldenrod' } else { 'ForestGreen' }
    }

    # GDI: skip reallocation when color unchanged; destroy only owned HICONs (not SystemIcons.Application)
    if ($color -ne $script:LastIconColor) {
        $wasOwned  = $script:IconIsOwned
        $oldHandle = if ($wasOwned -and $null -ne $script:icon -and $null -ne $script:icon.Icon) { $script:icon.Icon.Handle } else { [IntPtr]::Zero }
        $script:icon.Icon = New-StatusIcon $color   # updates $script:IconIsOwned
        $script:LastIconColor = $color
        if ($oldHandle -ne [IntPtr]::Zero) {
            try { [Win32Icon]::DestroyIcon($oldHandle) } catch {}
        }
    }

    $parts = @('OneDrive Sync')
    if ($conflicts -gt 0) { $parts += "$conflicts conflict(s)" }
    if ($null -ne $script:SyncStartedAt) {
        $elapsed = [int]([datetime]::Now - $script:SyncStartedAt).TotalMinutes
        $parts  += if ($elapsed -lt 1) { 'Syncing...' } else { "Syncing... ($elapsed min)" }
    }
    if ($script:LastPending -gt 0) { $parts += "$script:LastPending available" }
    $tip  = ($parts -join ' - ')
    $sync = Get-LastSyncText
    if ($sync) { $tip += " | $sync" }
    if ($tip.Length -gt 63) { $tip = $tip.Substring(0, 60) + '...' }
    $script:icon.Text = $tip
}

function Start-StatusRefresh {
    if ($null -ne $script:RefreshHandle) { return }
    $rs = $null; $ps = $null
    try {
        $rs = [System.Management.Automation.Runspaces.RunspaceFactory]::CreateRunspace()
        $rs.Open()
        $rs.SessionStateProxy.SetVariable('_CorePath', (Join-Path $PSScriptRoot 'onedrive-sync-core.ps1'))
        $rs.SessionStateProxy.SetVariable('_CfgPath',  (Join-Path $PSScriptRoot 'sync-config.ps1'))
        $ps = [powershell]::Create()
        $ps.Runspace = $rs
        [void]$ps.AddScript({
            . $_CorePath
            $cfg = Import-OdsConfig -ConfigPath $_CfgPath
            @(Get-OdsProjectStatus -Config $cfg) | ForEach-Object {
                [PSCustomObject]@{
                    Id           = $_.Id
                    Status       = $_.Status
                    Kind         = $_.Kind
                    Git          = $_.Git
                    LocalPresent = $_.LocalPresent
                    Conflicts    = [int]$_.Conflicts
                }
            }
        })
        $script:RefreshHandle = @{ PS = $ps; RS = $rs; Async = $ps.BeginInvoke() }
    } catch {
        try { if ($null -ne $ps) { $ps.Dispose() } } catch {}
        try { if ($null -ne $rs) { $rs.Dispose() } } catch {}
        $script:RefreshHandle = $null
    }
}

# ---------------------------------------------------------------------------
#  Per-project last-sync helper  (reads today+yesterday JSONL event log)
# ---------------------------------------------------------------------------
function Get-LastSyncPerProject {
    $result = @{}   # id -> ISO timestamp (keep most-recent)
    $days   = @(
        [datetime]::UtcNow
        [datetime]::UtcNow.AddDays(-1)
    )
    foreach ($day in $days) {
        $f = Join-Path $env:LOCALAPPDATA ("onedrive-sync\events\{0}.jsonl" -f $day.ToString('yyyy-MM-dd'))
        if (-not (Test-Path $f)) { continue }
        Get-Content $f -Tail 100 -ErrorAction SilentlyContinue | ForEach-Object {
            try {
                $ev = $_ | ConvertFrom-Json
                if ($ev -and $ev.event -eq 'bisync' -and $ev.id) {
                    if (-not $result.ContainsKey($ev.id) -or [string]$ev.ts -gt [string]$result[$ev.id]) {
                        $result[$ev.id] = $ev.ts
                    }
                }
            } catch {}
        }
    }
    $readable = @{}
    foreach ($id in $result.Keys) {
        try {
            $ago = [datetime]::Now - [datetime]::Parse($result[$id]).ToLocalTime()
            $readable[$id] = if ($ago.TotalMinutes -lt 1)  { 'just now' }
                        elseif ($ago.TotalMinutes -lt 60) { "$([int]$ago.TotalMinutes) min ago" }
                        elseif ($ago.TotalHours   -lt 24) { "$([int]$ago.TotalHours)h ago" }
                        else                               { 'Yesterday' }
        } catch { $readable[$id] = '' }
    }
    return $readable
}

# ---------------------------------------------------------------------------
#  Management window
# ---------------------------------------------------------------------------
$MainXaml = @'
<Window xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
        xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"
        Title="OneDrive Sync - Projects"
        Width="960" Height="560"
        WindowStartupLocation="CenterScreen"
        FontFamily="Segoe UI" FontSize="13"
        Background="#F5F5F5">
  <Window.Resources>
    <Style x:Key="Btn" TargetType="Button">
      <Setter Property="Foreground" Value="White"/>
      <Setter Property="BorderThickness" Value="0"/>
      <Setter Property="Padding" Value="14,6"/>
      <Setter Property="Margin" Value="3,0"/>
      <Setter Property="Cursor" Value="Hand"/>
      <Setter Property="FontFamily" Value="Segoe UI"/>
      <Setter Property="FontSize" Value="12"/>
      <Setter Property="Background" Value="#0078D4"/>
      <Setter Property="Template">
        <Setter.Value>
          <ControlTemplate TargetType="Button">
            <Border x:Name="bd" Background="{TemplateBinding Background}"
                    CornerRadius="4" Padding="{TemplateBinding Padding}">
              <ContentPresenter HorizontalAlignment="Center" VerticalAlignment="Center"/>
            </Border>
            <ControlTemplate.Triggers>
              <Trigger Property="IsMouseOver" Value="True">
                <Setter TargetName="bd" Property="Opacity" Value="0.85"/>
              </Trigger>
              <Trigger Property="IsEnabled" Value="False">
                <Setter TargetName="bd" Property="Background" Value="#CCCCCC"/>
              </Trigger>
            </ControlTemplate.Triggers>
          </ControlTemplate>
        </Setter.Value>
      </Setter>
    </Style>
  </Window.Resources>
  <Grid>
    <Grid.RowDefinitions>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="*"/>
      <RowDefinition Height="Auto"/>
    </Grid.RowDefinitions>

    <!-- Toolbar -->
    <Border Grid.Row="0" Background="White" BorderBrush="#E0E0E0" BorderThickness="0,0,0,1" Padding="10,8">
      <WrapPanel>
        <Button x:Name="btnSyncNow"    Content="Sync Now"     Background="#0078D4" Style="{StaticResource Btn}"/>
        <Button x:Name="btnPull"       Content="Pull Here"    Background="#107C10" Style="{StaticResource Btn}"/>
        <Button x:Name="btnOpenFolder" Content="Open Folder"  Background="#5C5C5C" Style="{StaticResource Btn}"/>
        <Rectangle Width="1" Fill="#E0E0E0" Margin="6,3"/>
        <Button x:Name="btnUnmap"      Content="Unmap"        Background="#5C5C5C" Style="{StaticResource Btn}"/>
        <Button x:Name="btnForget"     Content="Retire"       Background="#C50F1F" Style="{StaticResource Btn}"/>
        <Rectangle Width="1" Fill="#E0E0E0" Margin="6,3"/>
        <Button x:Name="btnConflicts"  Content="Conflicts"    Background="#5C5C5C" Style="{StaticResource Btn}"/>
        <Button x:Name="btnDiscover"   Content="Discover New" Background="#107C10" Style="{StaticResource Btn}"/>
        <Button x:Name="btnRetired"    Content="Show Retired" Background="#5C5C5C" Style="{StaticResource Btn}"/>
        <Button x:Name="btnRefresh"    Content="Refresh"      Background="#5C5C5C" Style="{StaticResource Btn}"/>
        <Rectangle Width="1" Fill="#E0E0E0" Margin="6,3"/>
        <Button x:Name="btnSettings"   Content="Settings..."    Background="#5C5C5C" Style="{StaticResource Btn}"/>
      </WrapPanel>
    </Border>

    <!-- Project grid -->
    <DataGrid x:Name="grid" Grid.Row="1"
              AutoGenerateColumns="False" IsReadOnly="True"
              SelectionMode="Extended" SelectionUnit="FullRow"
              CanUserReorderColumns="False" CanUserResizeRows="False"
              GridLinesVisibility="Horizontal" HorizontalGridLinesBrush="#F0F0F0"
              BorderThickness="0" Background="White"
              RowBackground="White" AlternatingRowBackground="#FAFAFA"
              RowHeight="34" FontSize="13">
      <DataGrid.ColumnHeaderStyle>
        <Style TargetType="DataGridColumnHeader">
          <Setter Property="Background" Value="White"/>
          <Setter Property="Foreground" Value="#888888"/>
          <Setter Property="FontSize" Value="11"/>
          <Setter Property="FontWeight" Value="SemiBold"/>
          <Setter Property="Padding" Value="8,6"/>
          <Setter Property="BorderBrush" Value="#E0E0E0"/>
          <Setter Property="BorderThickness" Value="0,0,0,1"/>
          <Setter Property="SeparatorBrush" Value="Transparent"/>
        </Style>
      </DataGrid.ColumnHeaderStyle>
      <DataGrid.Columns>
        <DataGridTemplateColumn Header="" Width="36" CanUserSort="False" CanUserResize="False">
          <DataGridTemplateColumn.CellTemplate>
            <DataTemplate>
              <Ellipse Width="10" Height="10" Fill="{Binding Dot}" Margin="4,0"/>
            </DataTemplate>
          </DataGridTemplateColumn.CellTemplate>
        </DataGridTemplateColumn>
        <DataGridTextColumn Header="PROJECT"   Binding="{Binding Id}"           Width="*"/>
        <DataGridTextColumn Header="STATUS"    Binding="{Binding Status}"        Width="80"/>
        <DataGridTextColumn Header="KIND"      Binding="{Binding Kind}"          Width="70"/>
        <DataGridTextColumn Header="GIT"       Binding="{Binding Git}"           Width="50"/>
        <DataGridTextColumn Header="LOCAL"     Binding="{Binding LocalPresent}"  Width="55"/>
        <DataGridTextColumn Header="CONFLICTS" Binding="{Binding Conflicts}"     Width="80"/>
        <DataGridTextColumn Header="LAST SYNC"  Binding="{Binding LastSync}"    Width="90"/>
      </DataGrid.Columns>
    </DataGrid>

    <!-- Status bar -->
    <Border Grid.Row="2" Background="White" BorderBrush="#E0E0E0" BorderThickness="0,1,0,0" Padding="12,7">
      <DockPanel>
        <TextBlock x:Name="lblCounts"   DockPanel.Dock="Right" Foreground="#888888" FontSize="12" VerticalAlignment="Center"/>
        <TextBlock x:Name="lblLastSync" Foreground="#555555"   FontSize="12" VerticalAlignment="Center"/>
      </DockPanel>
    </Border>
  </Grid>
</Window>
'@

function Show-OdsWindow {
    $cfg = Get-Cfg
    $win = New-WpfWindow $MainXaml

    $grid          = $win.FindName('grid')
    $btnSyncNow    = $win.FindName('btnSyncNow')
    $btnPull       = $win.FindName('btnPull')
    $btnOpenFolder = $win.FindName('btnOpenFolder')
    $btnUnmap      = $win.FindName('btnUnmap')
    $btnForget     = $win.FindName('btnForget')
    $btnConflicts  = $win.FindName('btnConflicts')
    $btnDiscover   = $win.FindName('btnDiscover')
    $btnRetired    = $win.FindName('btnRetired')
    $btnRefresh    = $win.FindName('btnRefresh')
    $btnSettings   = $win.FindName('btnSettings')
    $lblLastSync   = $win.FindName('lblLastSync')
    $lblCounts     = $win.FindName('lblCounts')

    function Refresh-Data {
        param([switch]$Force)
        $lastSyncs = Get-LastSyncPerProject
        $base = if (-not $Force -and $null -ne $script:CachedRows) {
            $script:CachedRows
        } else {
            $live = [object[]]@(@(Get-OdsProjectStatus -Config (Get-Cfg)) | ForEach-Object {
                [PSCustomObject]@{
                    Id           = $_.Id
                    Status       = $_.Status
                    Kind         = $_.Kind
                    Git          = if ($_.Git) { 'git' } else { 'plain' }
                    LocalPresent = if ($_.LocalPresent) { 'yes' } else { '-' }
                    Conflicts    = $_.Conflicts
                    Dot          = Get-StatusBrush $_.Status $_.Conflicts
                }
            })
            $script:CachedRows = $live
            Update-TrayIcon
            $live
        }
        # Merge fresh per-project last-sync times into display rows (creates new objects; does not mutate cache)
        $src = [object[]]@($base | ForEach-Object {
            [PSCustomObject]@{
                Dot          = $_.Dot
                Id           = $_.Id
                Status       = $_.Status
                Kind         = $_.Kind
                Git          = $_.Git
                LocalPresent = $_.LocalPresent
                Conflicts    = $_.Conflicts
                LastSync     = if ($lastSyncs.ContainsKey($_.Id)) { $lastSyncs[$_.Id] } else { '-' }
            }
        })
        $grid.ItemsSource = $src
        $lblLastSync.Text = Get-LastSyncText
        $active    = @($src | Where-Object { $_.Status -eq 'active' }).Count
        $conflicts = [int](($src | Measure-Object -Property Conflicts -Sum).Sum)
        $lblCounts.Text = "$active active" + $(if ($conflicts -gt 0) { ' | ' + $conflicts + ' conflict(s)' } else { '' })
    }

    function Get-SelectedIds { @($grid.SelectedItems | ForEach-Object { $_.Id }) }

    function Confirm-Action($msg) {
        [System.Windows.MessageBox]::Show($msg, 'OneDrive Sync', 'YesNo', 'Warning') -eq 'Yes'
    }

    $btnSyncNow.Add_Click({
        $ids = Get-SelectedIds
        if (-not $ids) { Invoke-Cli @('-SyncNow', '*') }
        else { foreach ($id in $ids) { Invoke-Cli @('-SyncNow', $id) } }
        $script:SyncStartedAt = [datetime]::Now
        Update-TrayIcon
        [System.Windows.MessageBox]::Show('Sync started in the background.', 'OneDrive Sync', 'OK', 'Information') | Out-Null
    })
    $btnPull.Add_Click({
        $ids = Get-SelectedIds
        if (-not $ids) { [System.Windows.MessageBox]::Show('Select one or more projects first.', 'OneDrive Sync', 'OK', 'Warning') | Out-Null; return }
        foreach ($id in $ids) { Invoke-Cli @('-Pull', $id) }
        Start-Sleep 1; Refresh-Data -Force
    })
    $btnOpenFolder.Add_Click({
        $ids = Get-SelectedIds
        if (-not $ids) { return }
        $allProjects = @(Get-OdsProjects -Config (Get-Cfg))
        foreach ($id in $ids) {
            $p = $allProjects | Where-Object id -eq $id | Select-Object -First 1
            if ($p -and (Test-Path $p.local)) { Start-Process explorer.exe $p.local }
        }
    })
    $btnUnmap.Add_Click({
        $ids = Get-SelectedIds
        if (-not $ids) { [System.Windows.MessageBox]::Show('Select projects to unmap.', 'OneDrive Sync', 'OK', 'Warning') | Out-Null; return }
        if (Confirm-Action "Unmap $($ids.Count) project(s) from this machine?`nThe OneDrive copy is kept.") {
            foreach ($id in $ids) { Invoke-Cli @('-Unmap', $id) }
            Start-Sleep 1; Refresh-Data -Force
        }
    })
    $btnForget.Add_Click({
        $ids = Get-SelectedIds
        if (-not $ids) { [System.Windows.MessageBox]::Show('Select projects to retire.', 'OneDrive Sync', 'OK', 'Warning') | Out-Null; return }
        if (Confirm-Action "Retire $($ids.Count) project(s) globally?`nThis tombstones them (reversible via Show Retired).") {
            foreach ($id in $ids) { Invoke-Cli @('-Forget', $id) }
            Start-Sleep 1; Refresh-Data -Force
        }
    })
    $btnConflicts.Add_Click({ Invoke-Cli @('-Conflicts') })
    $btnDiscover.Add_Click({ Show-OdsPicker; Refresh-Data -Force })
    $btnRetired.Add_Click({ Show-OdsRetired; Refresh-Data -Force })
    $btnRefresh.Add_Click({ Refresh-Data -Force })
    $btnSettings.Add_Click({ Show-OdsSettings; $script:Cfg = $null; Refresh-Data -Force })

    Refresh-Data
    [void]$win.ShowDialog()
}

# ---------------------------------------------------------------------------
#  Project picker (undecided projects)
# ---------------------------------------------------------------------------
$PickerXaml = @'
<Window xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
        xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"
        Title="Choose projects to sync here"
        Width="560" Height="420"
        WindowStartupLocation="CenterScreen"
        FontFamily="Segoe UI" FontSize="13">
  <Grid Margin="16">
    <Grid.RowDefinitions>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="*"/>
      <RowDefinition Height="Auto"/>
    </Grid.RowDefinitions>
    <TextBlock Grid.Row="0" Text="Select which projects to pull to this machine:"
               Foreground="#333333" Margin="0,0,0,10"/>
    <Border Grid.Row="1" BorderBrush="#E0E0E0" BorderThickness="1" CornerRadius="4">
      <ScrollViewer VerticalScrollBarVisibility="Auto">
        <StackPanel x:Name="pnlItems" Margin="8"/>
      </ScrollViewer>
    </Border>
    <StackPanel Grid.Row="2" Orientation="Horizontal" HorizontalAlignment="Right" Margin="0,12,0,0">
      <Button x:Name="btnSkipAll" Content="Skip All" Width="90" Margin="0,0,8,0"
              Background="#5C5C5C" Foreground="White" BorderThickness="0" Padding="0,7" Cursor="Hand"/>
      <Button x:Name="btnOk" Content="Sync Selected" Width="120"
              Background="#107C10" Foreground="White" BorderThickness="0" Padding="0,7" Cursor="Hand"/>
    </StackPanel>
  </Grid>
</Window>
'@

function Show-OdsPicker {
    $cfg   = Get-Cfg
    $state = Get-OdsMachineState
    $known = @(Get-OdsProjects -Config $cfg)
    $undecided = @($known | Where-Object {
        $state.active -notcontains $_.id -and
        $state.skip   -notcontains $_.id -and
        -not (Test-Path -LiteralPath $_.local)
    })
    if (-not $undecided) {
        [System.Windows.MessageBox]::Show('No new projects available.', 'OneDrive Sync', 'OK', 'Information') | Out-Null
        return
    }

    $win     = New-WpfWindow $PickerXaml
    $pnl     = $win.FindName('pnlItems')
    $btnOk   = $win.FindName('btnOk')
    $btnSkip = $win.FindName('btnSkipAll')

    foreach ($u in $undecided) {
        $cb         = New-Object System.Windows.Controls.CheckBox
        $cb.Content = $u.id
        $cb.Tag     = $u.id
        $cb.Margin  = [System.Windows.Thickness]::new(0, 4, 0, 4)
        $cb.FontSize = 13
        $pnl.Children.Add($cb) | Out-Null
    }

    $btnOk.Add_Click({
        if (Test-OdsSyncRunning) {
            [System.Windows.MessageBox]::Show('A sync is running - please try again in a moment.', 'OneDrive Sync', 'OK', 'Warning') | Out-Null
            return
        }
        foreach ($cb in $pnl.Children) {
            if ($cb.IsChecked) { Invoke-Cli @('-Pull', $cb.Tag) }
            else               { Set-OdsState -Id $cb.Tag -Status skip }
        }
        $win.Close()
    })
    $btnSkip.Add_Click({
        if (Test-OdsSyncRunning) {
            [System.Windows.MessageBox]::Show('A sync is running - please try again in a moment.', 'OneDrive Sync', 'OK', 'Warning') | Out-Null
            return
        }
        foreach ($cb in $pnl.Children) { Set-OdsState -Id $cb.Tag -Status skip }
        $win.Close()
    })

    [void]$win.ShowDialog()
}

# ---------------------------------------------------------------------------
#  Retired projects
# ---------------------------------------------------------------------------
$RetiredXaml = @'
<Window xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
        xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"
        Title="Retired projects"
        Width="500" Height="360"
        WindowStartupLocation="CenterScreen"
        FontFamily="Segoe UI" FontSize="13">
  <Grid Margin="16">
    <Grid.RowDefinitions>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="*"/>
      <RowDefinition Height="Auto"/>
    </Grid.RowDefinitions>
    <TextBlock Grid.Row="0" Text="Select retired projects to revive:"
               Foreground="#333333" Margin="0,0,0,10"/>
    <Border Grid.Row="1" BorderBrush="#E0E0E0" BorderThickness="1" CornerRadius="4">
      <ScrollViewer VerticalScrollBarVisibility="Auto">
        <StackPanel x:Name="pnlItems" Margin="8"/>
      </ScrollViewer>
    </Border>
    <StackPanel Grid.Row="2" Orientation="Horizontal" HorizontalAlignment="Right" Margin="0,12,0,0">
      <Button x:Name="btnRevive" Content="Revive Selected" Width="130"
              Background="#0078D4" Foreground="White" BorderThickness="0" Padding="0,7" Cursor="Hand"/>
    </StackPanel>
  </Grid>
</Window>
'@

function Show-OdsRetired {
    $cat = Get-OdsCatalog
    if (-not @($cat.forgotten)) {
        [System.Windows.MessageBox]::Show('No retired projects.', 'OneDrive Sync', 'OK', 'Information') | Out-Null
        return
    }

    $win    = New-WpfWindow $RetiredXaml
    $pnl    = $win.FindName('pnlItems')
    $btnRev = $win.FindName('btnRevive')

    foreach ($id in @($cat.forgotten)) {
        $cb         = New-Object System.Windows.Controls.CheckBox
        $cb.Content = $id
        $cb.Tag     = $id
        $cb.Margin  = [System.Windows.Thickness]::new(0, 4, 0, 4)
        $cb.FontSize = 13
        $pnl.Children.Add($cb) | Out-Null
    }

    $btnRev.Add_Click({
        foreach ($cb in $pnl.Children) {
            if ($cb.IsChecked) { Invoke-Cli @('-Pull', $cb.Tag) }
        }
        $win.Close()
    })

    [void]$win.ShowDialog()
}

# ---------------------------------------------------------------------------
#  Settings  (ProjectParents + PlainFolders local override)
# ---------------------------------------------------------------------------

function Select-OdsFolder {
    param([string]$Title = 'Select folder', [string]$InitialDir = '')
    $dlg = New-Object System.Windows.Forms.FolderBrowserDialog
    $dlg.Description = $Title
    $dlg.ShowNewFolderButton = $true
    if ($InitialDir -and (Test-Path $InitialDir)) { $dlg.SelectedPath = $InitialDir }
    if ($dlg.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { return $dlg.SelectedPath }
    return $null
}

function Save-OdsManagedConfig {
    param([string[]]$ProjectParents, $PlainFolders)
    $localPath = Join-Path $env:LOCALAPPDATA 'onedrive-sync\sync-config.local.ps1'
    $lines = @('# Auto-generated by onedrive-sync Settings window.')
    $lines += '$ProjectParents = @('
    foreach ($p in @($ProjectParents)) {
        if ($p) { $lines += "    '$($p.Replace("'","''"))'" }
    }
    $lines += ')'
    $lines += '$PlainFolders = @('
    foreach ($pf in @($PlainFolders)) {
        if ($pf) {
            $loc  = "$($pf.Local)".Replace("'","''")
            $dest = "$($pf.Dest)".Replace("'","''")
            $lines += "    @{ Local = '$loc'; Dest = '$dest' }"
        }
    }
    $lines += ')'
    [System.IO.File]::WriteAllText($localPath, ($lines -join [System.Environment]::NewLine), [System.Text.UTF8Encoding]::new($false))
    $script:Cfg = $null
}

$SettingsXaml = @'
<Window xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
        xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"
        Title="Settings - OneDrive Sync"
        Width="740" Height="540"
        WindowStartupLocation="CenterScreen"
        FontFamily="Segoe UI" FontSize="13"
        Background="#F5F5F5">
  <Grid Margin="16">
    <Grid.RowDefinitions>
      <RowDefinition Height="*"/>
      <RowDefinition Height="10"/>
      <RowDefinition Height="*"/>
      <RowDefinition Height="Auto"/>
    </Grid.RowDefinitions>

    <!-- Watch Roots -->
    <Grid Grid.Row="0">
      <Grid.RowDefinitions>
        <RowDefinition Height="Auto"/>
        <RowDefinition Height="*"/>
      </Grid.RowDefinitions>
      <Grid.ColumnDefinitions>
        <ColumnDefinition Width="*"/>
        <ColumnDefinition Width="86"/>
      </Grid.ColumnDefinitions>
      <TextBlock Grid.Row="0" Grid.ColumnSpan="2"
                 Text="Mirror Parents - OneDrive parent folders scanned for git projects:"
                 FontWeight="SemiBold" Foreground="#333333" Margin="0,0,0,6"/>
      <Border Grid.Row="1" Grid.Column="0" BorderBrush="#E0E0E0" BorderThickness="1" CornerRadius="3" Background="White">
        <ListBox x:Name="lstRoots" BorderThickness="0" Padding="4"
                 SelectionMode="Extended" Background="Transparent" FontSize="12"/>
      </Border>
      <StackPanel Grid.Row="1" Grid.Column="1" Margin="8,0,0,0">
        <Button x:Name="btnAddRoot"    Content="Add..."   Foreground="White" Background="#0078D4"
                BorderThickness="0" Padding="8,7" Margin="0,0,0,6" Cursor="Hand"/>
        <Button x:Name="btnRemoveRoot" Content="Remove" Foreground="White" Background="#C50F1F"
                BorderThickness="0" Padding="8,7" Cursor="Hand"/>
      </StackPanel>
    </Grid>

    <!-- Plain Folders -->
    <Grid Grid.Row="2">
      <Grid.RowDefinitions>
        <RowDefinition Height="Auto"/>
        <RowDefinition Height="*"/>
      </Grid.RowDefinitions>
      <Grid.ColumnDefinitions>
        <ColumnDefinition Width="*"/>
        <ColumnDefinition Width="86"/>
      </Grid.ColumnDefinitions>
      <TextBlock Grid.Row="0" Grid.ColumnSpan="2"
                 Text="Plain Folders - non-git folders synced by explicit local &lt;-&gt; OneDrive mapping:"
                 FontWeight="SemiBold" Foreground="#333333" Margin="0,0,0,6"/>
      <Border Grid.Row="1" Grid.Column="0" BorderBrush="#E0E0E0" BorderThickness="1" CornerRadius="3" Background="White">
        <DataGrid x:Name="gridPlain"
                  AutoGenerateColumns="False" IsReadOnly="True"
                  SelectionMode="Extended" SelectionUnit="FullRow"
                  CanUserResizeRows="False" GridLinesVisibility="Horizontal"
                  HorizontalGridLinesBrush="#F0F0F0"
                  HeadersVisibility="Column" BorderThickness="0"
                  Background="Transparent" FontSize="12">
          <DataGrid.ColumnHeaderStyle>
            <Style TargetType="DataGridColumnHeader">
              <Setter Property="Background"  Value="White"/>
              <Setter Property="Foreground"  Value="#888888"/>
              <Setter Property="FontSize"    Value="11"/>
              <Setter Property="FontWeight"  Value="SemiBold"/>
              <Setter Property="Padding"     Value="6,5"/>
              <Setter Property="BorderBrush" Value="#E0E0E0"/>
              <Setter Property="BorderThickness" Value="0,0,0,1"/>
              <Setter Property="SeparatorBrush" Value="Transparent"/>
            </Style>
          </DataGrid.ColumnHeaderStyle>
          <DataGrid.Columns>
            <DataGridTextColumn Header="LOCAL PATH"     Binding="{Binding Local}" Width="*"/>
            <DataGridTextColumn Header="ONEDRIVE PATH"  Binding="{Binding Dest}"  Width="*"/>
          </DataGrid.Columns>
        </DataGrid>
      </Border>
      <StackPanel Grid.Row="1" Grid.Column="1" Margin="8,0,0,0">
        <Button x:Name="btnAddPlain"    Content="Add..."   Foreground="White" Background="#0078D4"
                BorderThickness="0" Padding="8,7" Margin="0,0,0,6" Cursor="Hand"/>
        <Button x:Name="btnRemovePlain" Content="Remove" Foreground="White" Background="#C50F1F"
                BorderThickness="0" Padding="8,7" Cursor="Hand"/>
      </StackPanel>
    </Grid>

    <!-- Save / Cancel -->
    <StackPanel Grid.Row="3" Orientation="Horizontal" HorizontalAlignment="Right" Margin="0,12,0,0">
      <Button x:Name="btnSettingsCancel" Content="Cancel" Width="90" Margin="0,0,8,0"
              Foreground="White" Background="#5C5C5C" BorderThickness="0" Padding="0,7" Cursor="Hand"/>
      <Button x:Name="btnSettingsSave"   Content="Save"   Width="90"
              Foreground="White" Background="#0078D4" BorderThickness="0" Padding="0,7" Cursor="Hand"/>
    </StackPanel>
  </Grid>
</Window>
'@

function Show-OdsSettings {
    $cfg = Get-Cfg
    $win = New-WpfWindow $SettingsXaml

    $lstRoots          = $win.FindName('lstRoots')
    $gridPlain         = $win.FindName('gridPlain')
    $btnAddRoot        = $win.FindName('btnAddRoot')
    $btnRemoveRoot     = $win.FindName('btnRemoveRoot')
    $btnAddPlain       = $win.FindName('btnAddPlain')
    $btnRemovePlain    = $win.FindName('btnRemovePlain')
    $btnSettingsSave   = $win.FindName('btnSettingsSave')
    $btnSettingsCancel = $win.FindName('btnSettingsCancel')

    $roots = New-Object 'System.Collections.ObjectModel.ObservableCollection[string]'
    foreach ($r in @($cfg.ProjectParents)) { if ($r) { [void]$roots.Add($r) } }
    $lstRoots.ItemsSource = $roots

    $plains = New-Object 'System.Collections.ObjectModel.ObservableCollection[object]'
    foreach ($pf in @($cfg.PlainFolders)) {
        if ($pf) { [void]$plains.Add([PSCustomObject]@{ Local = $pf.Local; Dest = $pf.Dest }) }
    }
    $gridPlain.ItemsSource = $plains

    $btnAddRoot.Add_Click({
        $initDir = try { Get-OdsOneDriveRoot } catch { '' }
        $folder = Select-OdsFolder -Title 'Select OneDrive parent folder to scan for git projects' `
                                   -InitialDir $initDir
        if ($folder) { [void]$roots.Add($folder) }
    })
    $btnRemoveRoot.Add_Click({
        foreach ($item in @($lstRoots.SelectedItems)) { [void]$roots.Remove($item) }
    })
    $btnAddPlain.Add_Click({
        $local = Select-OdsFolder -Title 'Select LOCAL folder to sync' -InitialDir $env:USERPROFILE
        if (-not $local) { return }
        $initDir = try { Get-OdsOneDriveRoot } catch { '' }
        $dest = Select-OdsFolder -Title "Select OneDrive DESTINATION for '$($local | Split-Path -Leaf)'" `
                                 -InitialDir $initDir
        if (-not $dest) { return }
        [void]$plains.Add([PSCustomObject]@{ Local = $local; Dest = $dest })
    })
    $btnRemovePlain.Add_Click({
        foreach ($item in @($gridPlain.SelectedItems)) { [void]$plains.Remove($item) }
    })
    $btnSettingsSave.Add_Click({
        Save-OdsManagedConfig -ProjectParents @($roots) -PlainFolders @($plains)
        [System.Windows.MessageBox]::Show('Settings saved. Changes take effect on next sync.', 'OneDrive Sync', 'OK', 'Information') | Out-Null
        $win.Close()
    })
    $btnSettingsCancel.Add_Click({ $win.Close() })

    [void]$win.ShowDialog()
}

# ---------------------------------------------------------------------------
#  Tray icon  (WinForms NotifyIcon - WPF has no built-in tray support)
# ---------------------------------------------------------------------------
$icon = New-Object System.Windows.Forms.NotifyIcon
$icon.Icon = New-StatusIcon 'DimGray'
$icon.Text = 'OneDrive Sync'
$icon.Visible = $true

$menu = New-Object System.Windows.Forms.ContextMenuStrip
[void]$menu.Items.Add('Sync all now',           $null, { Invoke-Cli @('-SyncNow', '*'); $script:SyncStartedAt = [datetime]::Now; Update-TrayIcon })
[void]$menu.Items.Add('Manage...',              $null, { Show-OdsWindow })
[void]$menu.Items.Add('Choose new projects...', $null, { Show-OdsPicker })
[void]$menu.Items.Add('Settings...',           $null, { Show-OdsSettings; $script:Cfg = $null })
[void]$menu.Items.Add('-')
[void]$menu.Items.Add('Pause sync',             $null, { Invoke-Cli @('-Pause') })
[void]$menu.Items.Add('Resume sync',            $null, { Invoke-Cli @('-Resume') })
[void]$menu.Items.Add('Open log',               $null, { Start-Process notepad.exe (Join-Path $env:LOCALAPPDATA 'onedrive-sync\logs\sync.log') })
[void]$menu.Items.Add('-')
[void]$menu.Items.Add('Exit',                   $null, { $icon.Visible = $false; [System.Windows.Forms.Application]::Exit() })
$icon.ContextMenuStrip = $menu
$icon.Add_MouseClick({ if ($_.Button -eq 'Left') { Show-OdsWindow } })

# Background status refresh + pending project balloon.
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 15000
$script:LastPending = -1
$timer.Add_Tick({
    try {
        # Collect completed background status refresh
        if ($null -ne $script:RefreshHandle -and $script:RefreshHandle.Async.IsCompleted) {
            try {
                $results = $script:RefreshHandle.PS.EndInvoke($script:RefreshHandle.Async)
                $script:CachedRows = [object[]]@($results | ForEach-Object {
                    [PSCustomObject]@{
                        Id           = $_.Id
                        Status       = $_.Status
                        Kind         = $_.Kind
                        Git          = if ($_.Git) { 'git' } else { 'plain' }
                        LocalPresent = if ($_.LocalPresent) { 'yes' } else { '-' }
                        Conflicts    = $_.Conflicts
                        Dot          = Get-StatusBrush $_.Status $_.Conflicts
                    }
                })
                Update-TrayIcon
            } catch { } finally {
                try { $script:RefreshHandle.PS.Dispose() } catch {}
                try { $script:RefreshHandle.RS.Dispose() } catch {}
                $script:RefreshHandle = $null
            }
        }
        # Start next background refresh if idle
        Start-StatusRefresh
        # Detect new run-end events - sync-complete balloon + clear syncing indicator
        try {
            $lastRun = Get-OdsLastRunEndEvent
            if ($lastRun -and [string]$lastRun.ts -ne [string]$script:LastRunEndTs) {
                $script:LastRunEndTs  = $lastRun.ts
                $script:SyncStartedAt = $null
                Update-TrayIcon
                $summary = if ($lastRun.PSObject.Properties['summary'] -and $lastRun.summary) { $lastRun.summary } else { 'done' }
                $icon.BalloonTipTitle = 'OneDrive Sync'
                $icon.BalloonTipText  = "Sync complete - $summary"
                $icon.BalloonTipIcon  = 'Info'
                $icon.ShowBalloonTip(5000)
            }
        } catch {}
        # Clear stale syncing indicator if no run-end arrived within 30 minutes
        if ($null -ne $script:SyncStartedAt -and ([datetime]::Now - $script:SyncStartedAt).TotalMinutes -gt 30) {
            $script:SyncStartedAt = $null
            Update-TrayIcon
        }
        # Pending project balloon notification
        $pendingFile = Join-Path $env:LOCALAPPDATA 'onedrive-sync\pending.json'
        $n = 0
        if (Test-Path $pendingFile) {
            $parsed = Get-Content $pendingFile -Raw | ConvertFrom-Json
            $n = if ($null -eq $parsed) { 0 } else { @($parsed).Count }
        }
        if ($n -ne $script:LastPending) {
            $oldPending = $script:LastPending
            $script:LastPending = $n
            Update-TrayIcon
            if ($n -gt 0 -and $oldPending -ge 0) {
                $icon.BalloonTipTitle = 'OneDrive Sync'
                $icon.BalloonTipText  = "$n new project(s) available. Click to choose."
                $icon.ShowBalloonTip(5000)
            }
        }
    } catch { }
})
$timer.Start()
Start-StatusRefresh   # kick off first background fetch immediately; window will be instant after ~15s
# Seed LastRunEndTs with whatever run-end is already in today's log so we don't re-balloon on startup
try {
    $seed = Get-OdsLastRunEndEvent
    $script:LastRunEndTs = if ($seed) { $seed.ts } else { '' }
} catch { $script:LastRunEndTs = '' }
$icon.Add_BalloonTipClicked({ Show-OdsPicker })

if ($ShowWindow) { Show-OdsWindow }
[System.Windows.Forms.Application]::Run()
$icon.Visible = $false
