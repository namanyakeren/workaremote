# Install agent Worka Remote di Windows 7/8/10/11 (per user, tanpa admin).
#
#   powershell -ExecutionPolicy Bypass -File .\windows-install.ps1 `
#       -Server https://remote.perusahaan.com -Token TOKEN-ENROLL
#
# Setelah selesai, jalankan `worka-agent set-password` untuk unattended access.

param(
    [Parameter(Mandatory = $true)][string]$Server,
    [Parameter(Mandatory = $true)][string]$Token,
    [string]$Exe = ".\worka-agent.exe"
)

$ErrorActionPreference = "Stop"

$dir = Join-Path $env:LOCALAPPDATA "WorkaRemote"
$target = Join-Path $dir "worka-agent.exe"
New-Item -ItemType Directory -Force -Path $dir | Out-Null
Copy-Item -LiteralPath $Exe -Destination $target -Force

& $target enroll --server $Server --token $Token

$run = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
Set-ItemProperty -Path $run -Name "WorkaRemote" -Value "`"$target`""

Start-Process -FilePath $target -WindowStyle Hidden
Start-Sleep -Seconds 2

Write-Host ""
Write-Host "Terpasang di $dir"
Write-Host "ID mesin ini:"
& $target id
Write-Host ""
Write-Host "Unattended access : & `"$target`" set-password"
Write-Host "Kembali ke mode konfirmasi: & `"$target`" clear-password"
Write-Host ""
Write-Host "Catatan: agent berjalan sebagai user yang login. Layar UAC / login"
Write-Host "belum bisa dikendalikan (butuh service SYSTEM, belum termasuk)."
