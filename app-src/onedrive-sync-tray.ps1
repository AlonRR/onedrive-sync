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
$script:StartedAt             = [datetime]::Now  # startup time for stale-grace-period on fresh install
$script:WindowRefreshCallback = $null            # scriptblock set by open management window; cleared on close
$script:MainWin               = $null            # singleton management window (hidden when minimized to tray)
$script:WinForceClose         = $false           # $true when Exit intentionally closes the window
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
    # Build ONE properly-quoted command line: a project id/path with a space
    # (e.g. '3D printing\...') must reach the CLI as a single argument, not be split.
    $tokens = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $CliPath, '-NoUpdate') + $CliArgs
    $line = ($tokens | ForEach-Object {
        if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ }
    }) -join ' '
    Start-Process -FilePath $AppPwsh.Source -ArgumentList $line -WindowStyle Hidden
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

# Today + yesterday's parsed JSONL events, cached and re-read only when today's
# file changes (keyed on its mtime; yesterday's file is immutable once its day
# passes, so it needn't be in the key). Both display helpers below derive from
# this, so a timer tick's several Update-TrayIcon calls share one parse instead
# of each re-reading + re-parsing the logs on the UI thread.
$script:OdsEventCacheKey = $null
$script:OdsEventCacheVal = @()
function Get-OdsRecentEvents {
    try {
        $todayFile = Join-Path $env:LOCALAPPDATA ("onedrive-sync\events\{0}.jsonl" -f [datetime]::UtcNow.ToString('yyyy-MM-dd'))
        $key = if (Test-Path -LiteralPath $todayFile) {
            "$todayFile|" + [System.IO.File]::GetLastWriteTimeUtc($todayFile).Ticks
        } else { "$todayFile|none" }
        if ($key -eq $script:OdsEventCacheKey) { return $script:OdsEventCacheVal }
        $events = @(
            [datetime]::UtcNow.AddDays(-1)
            [datetime]::UtcNow
        ) | ForEach-Object {
            $evf = Join-Path $env:LOCALAPPDATA ("onedrive-sync\events\{0}.jsonl" -f $_.ToString('yyyy-MM-dd'))
            if (Test-Path -LiteralPath $evf) {
                Get-Content -LiteralPath $evf -Tail 200 -ErrorAction SilentlyContinue | ForEach-Object {
                    $e = $null
                    try { $e = $_ | ConvertFrom-Json } catch {}
                    # PS 7's ConvertFrom-Json parses ISO timestamps to [datetime]; 5.1 leaves
                    # them as strings. Re-stamp ts to a sortable ISO-UTC string so sort/compare/
                    # parse behave identically under whichever host runs the tray (it is 5.1
                    # today, but a manual pwsh launch would otherwise break the date math).
                    if ($e -and $e.PSObject.Properties['ts'] -and $e.ts -is [datetime]) {
                        $e.ts = $e.ts.ToUniversalTime().ToString('o')
                    }
                    $e
                }
            }
        }
        $script:OdsEventCacheVal = @($events | Where-Object { $_ })
        $script:OdsEventCacheKey = $key
        return $script:OdsEventCacheVal
    } catch { return @() }
}

function Get-OdsLastRunEndEvent {
    # Most-recent run-end across today+yesterday. Sort by ts (ToString('o') is
    # ISO-8601 UTC, so lexical = chronological); a bare -Last 1 on read order
    # would wrongly return yesterday's run-end when both days have one.
    @(Get-OdsRecentEvents | Where-Object { $_.event -eq 'run-end' }) |
        Sort-Object { [string]$_.ts } | Select-Object -Last 1
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
    $result = @{}   # id -> most-recent ISO timestamp
    foreach ($ev in Get-OdsRecentEvents) {
        # .event-first short-circuit keeps StrictMode from touching .id on
        # non-bisync events; guard .id since not every event carries it.
        if ($ev.event -eq 'bisync' -and $ev.PSObject.Properties['id'] -and $ev.id) {
            if (-not $result.ContainsKey($ev.id) -or [string]$ev.ts -gt [string]$result[$ev.id]) {
                $result[$ev.id] = $ev.ts
            }
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
#  Watch folder dialog
# ---------------------------------------------------------------------------
$WatchXaml = @'
<Window xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
        xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"
        Title="Watch Folder"
        Width="520" Height="300"
        WindowStartupLocation="CenterScreen"
        ResizeMode="NoResize"
        FontFamily="Segoe UI" FontSize="13">
  <Grid Margin="16">
    <Grid.RowDefinitions>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="*"/>
      <RowDefinition Height="Auto"/>
    </Grid.RowDefinitions>
    <TextBlock Grid.Row="0" Text="Choose a local folder and where it should live inside OneDrive. The folder name is preserved."
               Foreground="#555555" TextWrapping="Wrap" Margin="0,0,0,14"/>
    <Grid Grid.Row="1" Margin="0,0,0,10">
      <Grid.ColumnDefinitions>
        <ColumnDefinition Width="120"/>
        <ColumnDefinition Width="*"/>
        <ColumnDefinition Width="Auto"/>
      </Grid.ColumnDefinitions>
      <TextBlock Grid.Column="0" Text="Local folder:" VerticalAlignment="Center"/>
      <TextBox   Grid.Column="1" x:Name="txtLocal" IsReadOnly="True" Padding="5,4"
                 Background="#F8F8F8" BorderBrush="#CCCCCC" VerticalContentAlignment="Center"/>
      <Button    Grid.Column="2" x:Name="btnBrowseLocal" Content="Browse..." Margin="8,0,0,0"
                 Background="#5C5C5C" Foreground="White" BorderThickness="0" Padding="10,6" Cursor="Hand"/>
    </Grid>
    <Grid Grid.Row="2" Margin="0,0,0,6">
      <Grid.ColumnDefinitions>
        <ColumnDefinition Width="120"/>
        <ColumnDefinition Width="*"/>
        <ColumnDefinition Width="Auto"/>
      </Grid.ColumnDefinitions>
      <TextBlock Grid.Column="0" Text="OneDrive parent:" VerticalAlignment="Center"/>
      <TextBox   Grid.Column="1" x:Name="txtDest" IsReadOnly="True" Padding="5,4"
                 Background="#F8F8F8" BorderBrush="#CCCCCC" VerticalContentAlignment="Center"/>
      <Button    Grid.Column="2" x:Name="btnBrowseDest" Content="Browse..." Margin="8,0,0,0"
                 Background="#5C5C5C" Foreground="White" BorderThickness="0" Padding="10,6" Cursor="Hand"/>
    </Grid>
    <TextBlock Grid.Row="3" x:Name="txtPreview" Foreground="#007ACC" FontSize="11"
               TextWrapping="Wrap" Margin="0,2,0,0"/>
    <StackPanel Grid.Row="5" Orientation="Horizontal" HorizontalAlignment="Right">
      <Button x:Name="btnCancel"   Content="Cancel" Width="90" Margin="0,0,8,0"
              Background="#5C5C5C" Foreground="White" BorderThickness="0" Padding="0,7" Cursor="Hand"/>
      <Button x:Name="btnAddWatch" Content="Add"    Width="90"
              Background="#107C10" Foreground="White" BorderThickness="0" Padding="0,7" Cursor="Hand"/>
    </StackPanel>
  </Grid>
</Window>
'@

function Show-OdsWatch {
    $cfg = Get-Cfg
    $od  = try { Get-OdsOneDriveRoot } catch { '' }
    $win = New-WpfWindow $WatchXaml

    $txtLocal       = $win.FindName('txtLocal')
    $txtDest        = $win.FindName('txtDest')
    $txtPreview     = $win.FindName('txtPreview')
    $btnBrowseLocal = $win.FindName('btnBrowseLocal')
    $btnBrowseDest  = $win.FindName('btnBrowseDest')
    $btnAddWatch    = $win.FindName('btnAddWatch')
    $btnCancel      = $win.FindName('btnCancel')

    $blueBrush = [System.Windows.Media.BrushConverter]::new().ConvertFromString('#007ACC')
    $btnBrowseLocal.Add_Click({
        $f = Select-OdsFolder -Title 'Select the LOCAL git folder to watch' -InitialDir $env:USERPROFILE
        if ($f) {
            $txtLocal.Text = $f
            if ($txtDest.Text) {
                $leaf = Split-Path $f -Leaf
                $txtPreview.Foreground = $blueBrush
                $txtPreview.Text = "Will sync: $f  <->  $(Join-Path $txtDest.Text $leaf)"
            }
        }
    })
    $btnBrowseDest.Add_Click({
        $f = Select-OdsFolder -Title 'Select the OneDrive parent folder' -InitialDir $od
        if ($f) {
            $txtDest.Text = $f
            if ($txtLocal.Text) {
                $leaf = Split-Path $txtLocal.Text -Leaf
                $txtPreview.Foreground = $blueBrush
                $txtPreview.Text = "Will sync: $($txtLocal.Text)  <->  $(Join-Path $f $leaf)"
            }
        }
    })
    $btnCancel.Add_Click({ $win.Close() })
    $btnAddWatch.Add_Click({
        $local  = $txtLocal.Text.Trim()
        $parent = $txtDest.Text.Trim()
        if (-not $local -or -not $parent) {
            $txtPreview.Foreground = [System.Windows.Media.Brushes]::Crimson
            $txtPreview.Text = 'Select both a local folder and an OneDrive parent folder.'
            return
        }
        if (-not (Test-Path -LiteralPath $local)) {
            $txtPreview.Foreground = [System.Windows.Media.Brushes]::Crimson
            $txtPreview.Text = "Local folder not found: $local"
            return
        }
        $leaf = Split-Path $local -Leaf
        $dest = Join-Path $parent $leaf
        try {
            $id = Add-OdsWatchMapping -Local $local -Dest $dest -Config $cfg
            $win.Close()
            Invoke-Cli @('-Pull', $id)
        } catch {
            $txtPreview.Foreground = [System.Windows.Media.Brushes]::Crimson
            $txtPreview.Text = $_.Exception.Message
        }
    })

    [void]$win.ShowDialog()
}

# ---------------------------------------------------------------------------
#  Per-project settings dialog
# ---------------------------------------------------------------------------
$ProjectSettingsXaml = @'
<Window xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
        xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"
        Title="Project Settings"
        Width="580" Height="500"
        WindowStartupLocation="CenterScreen"
        ResizeMode="NoResize"
        FontFamily="Segoe UI" FontSize="13">
  <Grid Margin="16">
    <Grid.RowDefinitions>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="8"/>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="10"/>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="*"/>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="Auto"/>
    </Grid.RowDefinitions>
    <!-- Row 0: Header -->
    <DockPanel Grid.Row="0">
      <Border x:Name="bdgKind" DockPanel.Dock="Right" CornerRadius="3" Padding="7,3"
              Margin="10,0,0,0" VerticalAlignment="Center">
        <TextBlock x:Name="lblKind" FontSize="11" FontWeight="SemiBold" Foreground="White"/>
      </Border>
      <TextBlock x:Name="lblProject" FontWeight="SemiBold" FontSize="14" Foreground="#222222"
                 TextTrimming="CharacterEllipsis" VerticalAlignment="Center"/>
    </DockPanel>
    <!-- Row 2: PATHS label -->
    <TextBlock Grid.Row="2" Text="PATHS" FontSize="11" FontWeight="SemiBold"
               Foreground="#888888" Margin="0,0,0,4"/>
    <!-- Row 3: PATHS border -->
    <Border Grid.Row="3" BorderBrush="#E0E0E0" BorderThickness="1" CornerRadius="4" Padding="12,10">
      <Grid>
        <Grid.RowDefinitions>
          <RowDefinition Height="Auto"/>
          <RowDefinition Height="8"/>
          <RowDefinition Height="Auto"/>
          <RowDefinition Height="Auto"/>
        </Grid.RowDefinitions>
        <DockPanel Grid.Row="0">
          <TextBlock DockPanel.Dock="Left" Text="Local folder:" Width="115" VerticalAlignment="Center"/>
          <Button DockPanel.Dock="Right" x:Name="btnOpenLocal" Content="Open" Width="58"
                  Margin="6,0,0,0" Background="#5C5C5C" Foreground="White"
                  BorderThickness="0" Padding="0,6" Cursor="Hand"/>
          <Button DockPanel.Dock="Right" x:Name="btnBrowseLocal" Content="Browse..." Width="75"
                  Margin="6,0,0,0" Background="#5C5C5C" Foreground="White"
                  BorderThickness="0" Padding="0,6" Cursor="Hand"/>
          <TextBox x:Name="txtLocal" Padding="5,4" BorderBrush="#CCCCCC"
                   VerticalContentAlignment="Center"/>
        </DockPanel>
        <DockPanel Grid.Row="2">
          <TextBlock DockPanel.Dock="Left" Text="OneDrive folder:" Width="115" VerticalAlignment="Center"/>
          <Button DockPanel.Dock="Right" x:Name="btnOpenDest" Content="Open" Width="58"
                  Margin="6,0,0,0" Background="#5C5C5C" Foreground="White"
                  BorderThickness="0" Padding="0,6" Cursor="Hand"/>
          <Button DockPanel.Dock="Right" x:Name="btnBrowseDest" Content="Browse..." Width="75"
                  Margin="6,0,0,0" Background="#5C5C5C" Foreground="White"
                  BorderThickness="0" Padding="0,6" Cursor="Hand"/>
          <TextBox x:Name="txtDest" Padding="5,4" BorderBrush="#CCCCCC"
                   VerticalContentAlignment="Center"/>
        </DockPanel>
        <TextBlock Grid.Row="3" x:Name="lblMirrorNote" Visibility="Collapsed"
                   Text="Mirror projects follow the OneDrive mirroring law — paths are determined by the relative folder structure and cannot be changed independently."
                   Foreground="#888888" FontSize="11" TextWrapping="Wrap" Margin="0,8,0,0"/>
      </Grid>
    </Border>
    <!-- Row 5: SYNC SETTINGS label -->
    <TextBlock Grid.Row="5" Text="SYNC SETTINGS" FontSize="11" FontWeight="SemiBold"
               Foreground="#888888" Margin="0,0,0,4"/>
    <!-- Row 6: SYNC SETTINGS border -->
    <Border Grid.Row="6" BorderBrush="#E0E0E0" BorderThickness="1" CornerRadius="4" Padding="12,10">
      <Grid>
        <Grid.RowDefinitions>
          <RowDefinition Height="Auto"/>
          <RowDefinition Height="8"/>
          <RowDefinition Height="Auto"/>
        </Grid.RowDefinitions>
        <Grid.ColumnDefinitions>
          <ColumnDefinition Width="130"/>
          <ColumnDefinition Width="*"/>
        </Grid.ColumnDefinitions>
        <TextBlock Grid.Row="0" Grid.Column="0" Text="Compare mode:" VerticalAlignment="Center"/>
        <ComboBox  Grid.Row="0" Grid.Column="1" x:Name="cmbCompare">
          <ComboBoxItem Content="Default (from config)" Tag=""/>
          <ComboBoxItem Content="modtime (fast)"        Tag="modtime"/>
          <ComboBoxItem Content="checksum (thorough)"   Tag="checksum"/>
        </ComboBox>
        <TextBlock Grid.Row="2" Grid.Column="0" Text="Max delete %:" VerticalAlignment="Center"/>
        <StackPanel Grid.Row="2" Grid.Column="1" Orientation="Horizontal">
          <TextBox x:Name="txtMaxDelete" Width="70" VerticalContentAlignment="Center" Padding="4,3"/>
          <TextBlock Text="  (blank = default)" VerticalAlignment="Center" Foreground="#888888" FontSize="11"/>
        </StackPanel>
      </Grid>
    </Border>
    <!-- Row 8: Status -->
    <TextBlock Grid.Row="8" x:Name="lblStatus" Foreground="Crimson" FontSize="11"
               TextWrapping="Wrap" Margin="0,8,0,0"/>
    <!-- Row 9: Buttons -->
    <StackPanel Grid.Row="9" Orientation="Horizontal" HorizontalAlignment="Right" Margin="0,12,0,0">
      <Button x:Name="btnCancel" Content="Cancel" Width="90" Margin="0,0,8,0"
              Background="#5C5C5C" Foreground="White" BorderThickness="0" Padding="0,7" Cursor="Hand"/>
      <Button x:Name="btnSave" Content="Save" Width="90"
              Background="#0078D4" Foreground="White" BorderThickness="0" Padding="0,7" Cursor="Hand"/>
    </StackPanel>
  </Grid>
</Window>
'@

function Show-OdsProjectSettings {
    param([string]$ProjectId)

    $proj = Get-OdsProjects -Config (Get-Cfg) | Where-Object { $_.id -eq $ProjectId } | Select-Object -First 1
    if (-not $proj) {
        [System.Windows.MessageBox]::Show("Project '$ProjectId' not found.", 'OneDrive Sync', 'OK', 'Error') | Out-Null
        return
    }

    $s   = Get-OdsMachineState
    $win = New-WpfWindow $ProjectSettingsXaml

    $lblProject     = $win.FindName('lblProject')
    $lblKind        = $win.FindName('lblKind')
    $bdgKind        = $win.FindName('bdgKind')
    $txtLocal       = $win.FindName('txtLocal')
    $txtDest        = $win.FindName('txtDest')
    $btnBrowseLocal = $win.FindName('btnBrowseLocal')
    $btnBrowseDest  = $win.FindName('btnBrowseDest')
    $btnOpenLocal   = $win.FindName('btnOpenLocal')
    $btnOpenDest    = $win.FindName('btnOpenDest')
    $lblMirrorNote  = $win.FindName('lblMirrorNote')
    $cmb            = $win.FindName('cmbCompare')
    $txtMaxDelete   = $win.FindName('txtMaxDelete')
    $lblStatus      = $win.FindName('lblStatus')
    $btnSave        = $win.FindName('btnSave')
    $btnCancel      = $win.FindName('btnCancel')

    # Header
    $lblProject.Text = $ProjectId
    $lblKind.Text    = $proj.kind.ToUpper()
    $conv = [System.Windows.Media.BrushConverter]::new()
    $bdgKind.Background = switch ($proj.kind) {
        'watch'  { $conv.ConvertFromString('#107C10') }
        'plain'  { [System.Windows.Media.Brushes]::SteelBlue }
        default  { [System.Windows.Media.Brushes]::DimGray }
    }

    # Paths
    $txtLocal.Text = $proj.local
    $txtDest.Text  = $proj.dest

    if ($proj.kind -eq 'mirror') {
        $readOnlyBg = $conv.ConvertFromString('#F8F8F8')
        $txtLocal.IsReadOnly      = $true
        $txtDest.IsReadOnly       = $true
        $txtLocal.Background      = $readOnlyBg
        $txtDest.Background       = $readOnlyBg
        $btnBrowseLocal.IsEnabled = $false
        $btnBrowseDest.IsEnabled  = $false
        $lblMirrorNote.Visibility = [System.Windows.Visibility]::Visible
    }

    $openFolder = {
        param($txt)
        $p = $txt.Text
        if ($p -and (Test-Path -LiteralPath $p)) { Start-Process explorer.exe $p }
        else { $lblStatus.Text = "Folder not found: $p" }
    }
    $btnOpenLocal.Add_Click({ & $openFolder $txtLocal })
    $btnOpenDest.Add_Click({  & $openFolder $txtDest  })

    $od = try { Get-OdsOneDriveRoot } catch { '' }
    $btnBrowseLocal.Add_Click({
        $initDir = try { Split-Path $txtLocal.Text -Parent } catch { $env:USERPROFILE }
        if (-not $initDir -or -not (Test-Path $initDir)) { $initDir = $env:USERPROFILE }
        $f = Select-OdsFolder -Title 'Select new local folder' -InitialDir $initDir
        if ($f) { $txtLocal.Text = $f; $lblStatus.Text = '' }
    })
    $btnBrowseDest.Add_Click({
        $initDir = try { Split-Path $txtDest.Text -Parent } catch { $od }
        if (-not $initDir -or -not (Test-Path $initDir)) { $initDir = $od }
        $f = Select-OdsFolder -Title 'Select new OneDrive folder' -InitialDir $initDir
        if ($f) { $txtDest.Text = $f; $lblStatus.Text = '' }
    })

    # Sync settings
    $curMode = if ($null -ne $s.compare.PSObject.Properties[$ProjectId]) { $s.compare.$ProjectId } else { '' }
    foreach ($item in $cmb.Items) {
        if ($item.Tag -eq $curMode) { $cmb.SelectedItem = $item; break }
    }
    if ($null -eq $cmb.SelectedItem) { $cmb.SelectedIndex = 0 }
    if ($null -ne $s.maxDelete.PSObject.Properties[$ProjectId]) {
        $txtMaxDelete.Text = [string]($s.maxDelete.$ProjectId)
    }

    $btnCancel.Add_Click({ $win.Close() })
    $btnSave.Add_Click({
        $lblStatus.Text = ''
        try {
            $mode  = $cmb.SelectedItem.Tag
            $mdTxt = $txtMaxDelete.Text.Trim()
            $md    = if ($mdTxt -match '^\d+$') { [int]$mdTxt } else { $null }

            $newLocal = $txtLocal.Text.Trim().TrimEnd('\')
            $newDest  = $txtDest.Text.Trim().TrimEnd('\')
            $curLocal = $proj.local.TrimEnd('\')
            $curDest  = $proj.dest.TrimEnd('\')

            $pathsChanged = ($proj.kind -ne 'mirror') -and ($newLocal -ne $curLocal -or $newDest -ne $curDest)
            $effectiveId  = $ProjectId

            if ($pathsChanged) {
                if (-not $newLocal) { throw 'Local folder path cannot be empty.' }
                if (-not $newDest)  { throw 'OneDrive folder path cannot be empty.' }
                if ((Test-OdsIsProtectedRoot $newLocal) -or (Test-OdsIsProtectedRoot $newDest)) {
                    throw 'Local or OneDrive folder is a protected root — refused.'
                }
                if (Test-OdsOverlap $newLocal $newDest) {
                    throw 'Local and OneDrive folders overlap — refused (would self-sync).'
                }

                if ($proj.kind -eq 'watch') {
                    $up = $env:USERPROFILE.TrimEnd('\')
                    $newLocalRel = Get-OdsRelUnder -Full $newLocal -Root $up
                    $newDestRel  = Get-OdsRelUnder -Full $newDest  -Root $od
                    if ([string]::IsNullOrEmpty($newLocalRel)) { throw "Local folder must be a folder UNDER your user profile ($up)." }
                    if ([string]::IsNullOrEmpty($newDestRel))  { throw "OneDrive folder must be a folder UNDER the OneDrive root ($od)." }

                    $catalog = Get-OdsCatalog
                    $entry = @($catalog.entries) | Where-Object { $_.id -eq $ProjectId } | Select-Object -First 1
                    if (-not $entry) { throw "Catalog entry not found for '$ProjectId'." }
                    $entry.localRel = $newLocalRel
                    $entry.destRel  = $newDestRel
                    $entry.id       = $newDestRel
                    Save-OdsCatalog $catalog

                    $effectiveId = $newDestRel
                    Move-OdsProjectState -FromId $ProjectId -ToId $effectiveId

                } elseif ($proj.kind -eq 'plain') {
                    $cfg = Get-Cfg
                    $updatedPlains = @($cfg.PlainFolders) | ForEach-Object {
                        if ($null -ne $_ -and
                            $_.Local.TrimEnd('\') -eq $curLocal -and
                            $_.Dest.TrimEnd('\')  -eq $curDest) {
                            [PSCustomObject]@{ Local = $newLocal; Dest = $newDest }
                        } else { $_ }
                    }
                    Save-OdsManagedConfig -ProjectParents @($cfg.ProjectParents) -PlainFolders $updatedPlains

                    # A plain project's id is its Dest relative to the OneDrive root (else full
                    # Dest) — must match Get-OdsProjects, else a Dest change orphans its state.
                    $newId = Get-OdsRelUnder -Full $newDest -Root $od
                    if (-not $newId) { $newId = $newDest }
                    $effectiveId = $newId
                    Move-OdsProjectState -FromId $ProjectId -ToId $effectiveId
                }

                # Any path change invalidates the bisync baseline (workdir keyed by id-hash):
                #  - id changed (Dest moved): the old workdir is now orphaned -> drop it;
                #    the new id has no baseline and resyncs cleanly on its own.
                #  - id unchanged (Local moved): the stale baseline would mis-compare the new
                #    folder -> drop it to force a clean resync.
                Reset-OdsBaseline -Id $ProjectId
                if ($effectiveId -ne $ProjectId) { Reset-OdsBaseline -Id $effectiveId }
            }

            Set-OdsProjectSettings -Id $effectiveId -CompareMode $mode -MaxDelete $md
            $win.Close()
        } catch {
            $lblStatus.Text = $_.Exception.Message
        }
    })

    [void]$win.ShowDialog()
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
        <Button x:Name="btnWatch"      Content="Watch..."     Background="#107C10" Style="{StaticResource Btn}"/>
        <Button x:Name="btnDiscover"   Content="Discover New" Background="#107C10" Style="{StaticResource Btn}"/>
        <Button x:Name="btnRetired"    Content="Show Retired" Background="#5C5C5C" Style="{StaticResource Btn}"/>
        <Button x:Name="btnRefresh"    Content="Refresh"      Background="#5C5C5C" Style="{StaticResource Btn}"/>
        <Rectangle Width="1" Fill="#E0E0E0" Margin="6,3"/>
        <Button x:Name="btnProjSettings" Content="Project..."   Background="#5C5C5C" Style="{StaticResource Btn}"/>
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
        <DataGridTextColumn Header="COMPARE"   Binding="{Binding Compare}"       Width="80"/>
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
    if ($null -ne $script:MainWin) {
        # The window is shown modally (ShowDialog) and only Hidden on close, so the
        # modal loop is still live — calling .Show() again throws. Re-reveal via Visibility.
        $script:MainWin.Visibility  = [System.Windows.Visibility]::Visible
        $script:MainWin.WindowState = [System.Windows.WindowState]::Normal
        $script:MainWin.Activate()
        return
    }
    $cfg = Get-Cfg
    $win = New-WpfWindow $MainXaml
    $script:MainWin = $win

    $grid          = $win.FindName('grid')
    $btnSyncNow    = $win.FindName('btnSyncNow')
    $btnPull       = $win.FindName('btnPull')
    $btnOpenFolder = $win.FindName('btnOpenFolder')
    $btnUnmap      = $win.FindName('btnUnmap')
    $btnForget     = $win.FindName('btnForget')
    $btnConflicts  = $win.FindName('btnConflicts')
    $btnWatch      = $win.FindName('btnWatch')
    $btnDiscover   = $win.FindName('btnDiscover')
    $btnRetired    = $win.FindName('btnRetired')
    $btnRefresh      = $win.FindName('btnRefresh')
    $btnProjSettings = $win.FindName('btnProjSettings')
    $btnSettings   = $win.FindName('btnSettings')
    $lblLastSync   = $win.FindName('lblLastSync')
    $lblCounts     = $win.FindName('lblCounts')
    $idleTextBrush = [System.Windows.Media.BrushConverter]::new().ConvertFromString('#555555')

    function Refresh-Data {
        param([switch]$Force)
        if ($Force) {
            # Async path: null cache, restart background scan, show placeholder, return immediately.
            $script:CachedRows = $null
            if ($null -ne $script:RefreshHandle) {
                # Stop the in-flight pipeline before disposing, else the runspace
                # worker thread is abandoned (a slow leak on every force-refresh).
                try { $script:RefreshHandle.PS.Stop() }    catch {}
                try { $script:RefreshHandle.PS.Dispose() } catch {}
                try { $script:RefreshHandle.RS.Dispose() } catch {}
                $script:RefreshHandle = $null
            }
            Start-StatusRefresh
            $timer.Interval  = 2000   # poll quickly until the scan completes
            $lblCounts.Text  = 'Refreshing...'
            return
        }
        # Non-force path (includes initial open): use cache if warm, else scan once synchronously.
        $lastSyncs = Get-LastSyncPerProject
        $base = if ($null -ne $script:CachedRows) {
            $script:CachedRows
        } else {
            $cmpState = (Get-OdsMachineState).compare
            $live = [object[]]@(@(Get-OdsProjectStatus -Config (Get-Cfg)) | ForEach-Object {
                $cmpMode = if ($null -ne $cmpState.PSObject.Properties[$_.Id]) { $cmpState.$($_.Id) } else { 'default' }
                [PSCustomObject]@{
                    Id           = $_.Id
                    Status       = $_.Status
                    Kind         = $_.Kind
                    Git          = if ($_.Git) { 'git' } else { 'plain' }
                    LocalPresent = if ($_.LocalPresent) { 'yes' } else { '-' }
                    Conflicts    = $_.Conflicts
                    Compare      = $cmpMode
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
                Compare      = $_.Compare
                LastSync     = if ($lastSyncs.ContainsKey($_.Id)) { $lastSyncs[$_.Id] } else { '-' }
            }
        })
        $preSelected = @($grid.SelectedItems | ForEach-Object { $_.Id })
        $grid.ItemsSource = $src
        foreach ($item in $grid.Items) {
            if ($preSelected -contains $item.Id) { $grid.SelectedItems.Add($item) }
        }
        $lblLastSync.Foreground = $idleTextBrush
        $lblLastSync.Text = Get-LastSyncText
        $active    = @($src | Where-Object { $_.Status -eq 'active' }).Count
        $conflicts = [int](($src | Measure-Object -Property Conflicts -Sum).Sum)
        $lblCounts.Text = "$active active" + $(if ($conflicts -gt 0) { ' | ' + $conflicts + ' conflict(s)' } else { '' })
    }

    function Get-SelectedIds { @($grid.SelectedItems | ForEach-Object { $_.Id }) }

    function Confirm-Action($msg) {
        [System.Windows.MessageBox]::Show($msg, 'OneDrive Sync', 'YesNo', 'Warning') -eq 'Yes'
    }
    function Set-WinStatus {
        param([string]$Msg, [System.Windows.Media.Brush]$Brush = $null)
        $lblLastSync.Foreground = if ($null -eq $Brush) { $idleTextBrush } else { $Brush }
        $lblLastSync.Text = $Msg
    }
    function Update-ButtonStates {
        $n = $grid.SelectedItems.Count
        $btnPull.IsEnabled         = $n -gt 0
        $btnOpenFolder.IsEnabled   = $n -gt 0
        $btnUnmap.IsEnabled        = $n -gt 0
        $btnForget.IsEnabled       = $n -gt 0
        $btnProjSettings.IsEnabled = $n -eq 1
    }
    Update-ButtonStates
    $grid.Add_SelectionChanged({ Update-ButtonStates })

    $btnSyncNow.Add_Click({
        $ids = Get-SelectedIds
        if (-not $ids) { Invoke-Cli @('-SyncNow', '*') }
        else { foreach ($id in $ids) { Invoke-Cli @('-SyncNow', $id) } }
        $script:SyncStartedAt = [datetime]::Now
        Update-TrayIcon
        Set-WinStatus 'Syncing...'
    })
    $btnPull.Add_Click({
        $ids = Get-SelectedIds
        if (-not $ids) { Set-WinStatus 'Select one or more projects first.' ([System.Windows.Media.Brushes]::Crimson); return }
        foreach ($id in $ids) { Invoke-Cli @('-Pull', $id) }
        Refresh-Data -Force
    })
    $btnOpenFolder.Add_Click({
        $ids = Get-SelectedIds
        if (-not $ids) { return }
        $allProjects = @(Get-OdsProjects -Config (Get-Cfg))
        foreach ($id in $ids) {
            $p = $allProjects | Where-Object id -eq $id | Select-Object -First 1
            if ($p -and (Test-Path -LiteralPath $p.local)) { Start-Process explorer.exe -ArgumentList "`"$($p.local)`"" }
        }
    })
    $btnUnmap.Add_Click({
        $ids = Get-SelectedIds
        if (-not $ids) { Set-WinStatus 'Select projects to unmap.' ([System.Windows.Media.Brushes]::Crimson); return }
        if (Confirm-Action "Unmap $($ids.Count) project(s) from this machine?`nThe OneDrive copy is kept.") {
            foreach ($id in $ids) { Invoke-Cli @('-Unmap', $id) }
            Refresh-Data -Force
        }
    })
    $btnForget.Add_Click({
        $ids = Get-SelectedIds
        if (-not $ids) { Set-WinStatus 'Select projects to retire.' ([System.Windows.Media.Brushes]::Crimson); return }
        if (Confirm-Action "Retire $($ids.Count) project(s) globally?`nThis tombstones them (reversible via Show Retired).") {
            foreach ($id in $ids) { Invoke-Cli @('-Forget', $id) }
            Refresh-Data -Force
        }
    })
    $btnConflicts.Add_Click({ Invoke-Cli @('-Conflicts') })
    $btnWatch.Add_Click({ Show-OdsWatch; Refresh-Data -Force })
    $btnDiscover.Add_Click({ Show-OdsPicker; Refresh-Data -Force })
    $btnRetired.Add_Click({ Show-OdsRetired; Refresh-Data -Force })
    $btnRefresh.Add_Click({ Refresh-Data -Force })
    $btnSettings.Add_Click({ Show-OdsSettings; $script:Cfg = $null; Refresh-Data -Force })
    $btnProjSettings.Add_Click({
        $ids = @(Get-SelectedIds)
        if ($ids.Count -ne 1) { Set-WinStatus 'Select exactly one project.' ([System.Windows.Media.Brushes]::Crimson); return }
        Show-OdsProjectSettings -ProjectId $ids[0]
        Refresh-Data -Force
    })
    $grid.Add_MouseDoubleClick({
        $ids = @(Get-SelectedIds)
        if ($ids.Count -eq 1) { Show-OdsProjectSettings -ProjectId $ids[0]; Refresh-Data -Force }
    })

    # Register callback so the timer tick can push completed background scans into the grid.
    $script:WindowRefreshCallback = { Refresh-Data }
    $win.Add_Closed({
        $script:WindowRefreshCallback = $null
        $script:MainWin       = $null
        $script:WinForceClose = $false
    })
    $win.Add_Closing({
        param($s, $e)
        if (-not $script:WinForceClose) {
            $e.Cancel = $true
            $win.Hide()
        }
    })
    $win.Add_StateChanged({
        if ($win.WindowState -eq [System.Windows.WindowState]::Minimized) {
            $win.Hide()
        }
    })

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
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="*"/>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="Auto"/>
    </Grid.RowDefinitions>
    <TextBlock Grid.Row="0" Text="Select which projects to pull to this machine:"
               Foreground="#333333" Margin="0,0,0,6"/>
    <CheckBox Grid.Row="1" x:Name="cbSelectAll" Content="Select All"
              Margin="4,0,0,8" FontSize="13"/>
    <Border Grid.Row="2" BorderBrush="#E0E0E0" BorderThickness="1" CornerRadius="4">
      <ScrollViewer VerticalScrollBarVisibility="Auto">
        <StackPanel x:Name="pnlItems" Margin="8"/>
      </ScrollViewer>
    </Border>
    <TextBlock Grid.Row="3" x:Name="lblPickerStatus" Foreground="Crimson" FontSize="11"
               Margin="0,6,0,0" TextWrapping="Wrap"/>
    <StackPanel Grid.Row="4" Orientation="Horizontal" HorizontalAlignment="Right" Margin="0,12,0,0">
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
    if (-not $undecided) { return }

    $win             = New-WpfWindow $PickerXaml
    $pnl             = $win.FindName('pnlItems')
    $btnOk           = $win.FindName('btnOk')
    $btnSkip         = $win.FindName('btnSkipAll')
    $cbAll           = $win.FindName('cbSelectAll')
    $lblPickerStatus = $win.FindName('lblPickerStatus')

    foreach ($u in $undecided) {
        $cb         = New-Object System.Windows.Controls.CheckBox
        $cb.Content = $u.id
        $cb.Tag     = $u.id
        $cb.Margin  = [System.Windows.Thickness]::new(0, 4, 0, 4)
        $cb.FontSize = 13
        $pnl.Children.Add($cb) | Out-Null
    }

    $cbAll.Add_Checked({   foreach ($cb in $pnl.Children) { $cb.IsChecked = $true  } })
    $cbAll.Add_Unchecked({ foreach ($cb in $pnl.Children) { $cb.IsChecked = $false } })

    $btnOk.Add_Click({
        if (Test-OdsSyncRunning) { $lblPickerStatus.Text = 'A sync is running - please try again in a moment.'; return }
        foreach ($cb in $pnl.Children) {
            if ($cb.IsChecked) { Invoke-Cli @('-Pull', $cb.Tag) }
            else               { Set-OdsState -Id $cb.Tag -Status skip }
        }
        $win.Close()
    })
    $btnSkip.Add_Click({
        if (Test-OdsSyncRunning) { $lblPickerStatus.Text = 'A sync is running - please try again in a moment.'; return }
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
    if (-not @($cat.forgotten)) { return }

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

function ConvertTo-OdsPsLiteral {
    # Serialize a config override value (string / number / bool / array of those)
    # back to PowerShell source, to round-trip hand-set overrides verbatim.
    param($v)
    if ($null -eq $v) { return '$null' }
    if ($v -is [bool]) { if ($v) { return '$true' } else { return '$false' } }
    if ($v -is [int] -or $v -is [long] -or $v -is [double] -or $v -is [decimal]) { return "$v" }
    if ($v -isnot [string] -and ($v -is [array] -or $v -is [System.Collections.IEnumerable])) {
        return '@(' + ((@($v) | ForEach-Object { ConvertTo-OdsPsLiteral $_ }) -join ', ') + ')'
    }
    return "'" + ("$v".Replace("'", "''")) + "'"
}

function Save-OdsManagedConfig {
    param([string[]]$ProjectParents, $PlainFolders)
    $localPath = Join-Path $env:LOCALAPPDATA 'onedrive-sync\sync-config.local.ps1'
    # Capture any OTHER override variables the user set by hand, so rewriting the two
    # GUI-managed lists doesn't silently wipe them.
    $others = & {
        $captured = [ordered]@{}
        if (Test-Path -LiteralPath $localPath) {
            try { . $localPath } catch {}
            foreach ($n in @('WatchRoots','ExcludeDirs','ExcludeFiles','SyncAnywayList','VersionRetentionDays',
                             'VersionMaxGB','MaxDeletePercent','IdleStabilitySeconds','CompareMode','NoiseThreshold',
                             'QuietRunsToRevert','RetryMaxAttempts','RetryBackoff','RetryMaxWaitSeconds',
                             'DeferEscalateCycles','RcloneTransfers','ToolUpdateMode','RunTimeBudget')) {
                $gv = Get-Variable -Name $n -ErrorAction SilentlyContinue
                if ($gv -and $null -ne $gv.Value) { $captured[$n] = $gv.Value }
            }
        }
        $captured
    }
    $lines = @('# Auto-generated by onedrive-sync Settings window. ProjectParents/PlainFolders are')
    $lines += '# GUI-managed; other override variables below are preserved across saves.'
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
    foreach ($k in $others.Keys) { $lines += ('${0} = {1}' -f $k, (ConvertTo-OdsPsLiteral $others[$k])) }
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
    <TextBlock Grid.Row="3" x:Name="lblSettingsStatus" FontSize="11" Margin="0,8,0,0" TextWrapping="Wrap"/>
    <StackPanel Grid.Row="4" Orientation="Horizontal" HorizontalAlignment="Right" Margin="0,12,0,0">
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
    $lblSettingsStatus = $win.FindName('lblSettingsStatus')

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
        try {
            Save-OdsManagedConfig -ProjectParents @($roots) -PlainFolders @($plains)
            $win.Close()
        } catch {
            $lblSettingsStatus.Foreground = [System.Windows.Media.Brushes]::Crimson
            $lblSettingsStatus.Text = $_.Exception.Message
        }
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
[void]$menu.Items.Add('Exit',                   $null, {
    $icon.Visible = $false
    if ($null -ne $script:MainWin) { $script:WinForceClose = $true; $script:MainWin.Close() }
    [System.Windows.Forms.Application]::Exit()
})
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
                $cmpState = (Get-OdsMachineState).compare
                $script:CachedRows = [object[]]@($results | ForEach-Object {
                    $cmpMode = if ($null -ne $cmpState.PSObject.Properties[$_.Id]) { $cmpState.$($_.Id) } else { 'default' }
                    [PSCustomObject]@{
                        Id           = $_.Id
                        Status       = $_.Status
                        Kind         = $_.Kind
                        Git          = if ($_.Git) { 'git' } else { 'plain' }
                        LocalPresent = if ($_.LocalPresent) { 'yes' } else { '-' }
                        Conflicts    = $_.Conflicts
                        Compare      = $cmpMode
                        Dot          = Get-StatusBrush $_.Status $_.Conflicts
                    }
                })
                Update-TrayIcon
                if ($timer.Interval -lt 15000) { $timer.Interval = 15000 }  # restore after a force-refresh
                if ($null -ne $script:WindowRefreshCallback) { & $script:WindowRefreshCallback }
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
