<#
.SYNOPSIS
  Roll back to the PowerShell tool: remove the ods scheduled tasks, stop the ods
  tray, and re-enable the PowerShell tasks. The ods binaries are left in place.
#>
$ErrorActionPreference = 'Continue'

foreach ($t in 'ods-sync', 'ods-tray') {
    if (Get-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue) {
        Unregister-ScheduledTask -TaskName $t -Confirm:$false
        Write-Host "removed task $t"
    }
}
Get-Process ods-gui -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue

foreach ($t in 'OneDriveCodeSync', 'OneDriveCodeSyncTray') {
    if (Get-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue) {
        try { Enable-ScheduledTask -TaskName $t -ErrorAction Stop | Out-Null; Write-Host "re-enabled $t" }
        catch { Write-Warning "couldn't re-enable $t ($($_.Exception.Message.Trim())); run elevated" }
    }
}
Write-Host "rolled back to the PowerShell tool." -ForegroundColor Green
