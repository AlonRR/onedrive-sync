<#
.SYNOPSIS
  One-line bootstrap installer — fetches install.ps1 and runs it against the
  latest GitHub Release (no clone, no build). Intended for:

      irm https://raw.githubusercontent.com/AlonRR/onedrive-sync/main/scripts/get.ps1 | iex
#>
$ErrorActionPreference = 'Stop'
$inst = Join-Path $env:TEMP 'ods-install.ps1'
Invoke-WebRequest -Uri 'https://raw.githubusercontent.com/AlonRR/onedrive-sync/main/scripts/install.ps1' -OutFile $inst
& $inst -FromRelease
