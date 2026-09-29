# Build dist\F1R3Gaze-<ver>-x64.msi from built binaries, signing the
# executables and the installer when a certificate is provided.
#   packaging\windows\package.ps1 -Version 0.1.0 -Bin target\release
# Signing env (optional): WINDOWS_CERT_PFX (base64 .pfx), WINDOWS_CERT_PASSWORD,
# WINDOWS_TIMESTAMP_URL (default http://timestamp.digicert.com).
# Requires: the WiX v4 CLI (dotnet tool install --global wix) and signtool.
param([Parameter(Mandatory)][string]$Version, [Parameter(Mandatory)][string]$Bin, [string]$Dist = "dist")
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$icons = Resolve-Path "$here\..\icons"
New-Item -ItemType Directory -Force $Dist | Out-Null
$Dist = Resolve-Path $Dist
$stage = Join-Path ([IO.Path]::GetTempPath()) ("f1r3gaze-" + [Guid]::NewGuid())
New-Item -ItemType Directory $stage | Out-Null
Copy-Item "$Bin\f1r3gaze.exe", "$Bin\f1r3c.exe" $stage

# MSI versions are numeric: 0.1.0-rc.1 installs as 0.1.0.
$msiVersion = ($Version -split '[-+]')[0]

$signtool = $null
if ($env:WINDOWS_CERT_PFX) {
  $pfx = Join-Path $stage "cert.pfx"
  [IO.File]::WriteAllBytes($pfx, [Convert]::FromBase64String($env:WINDOWS_CERT_PFX))
  $signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" |
    Sort-Object FullName -Descending | Select-Object -First 1
  $ts = if ($env:WINDOWS_TIMESTAMP_URL) { $env:WINDOWS_TIMESTAMP_URL } else { "http://timestamp.digicert.com" }
  function Sign($f) {
    & $signtool.FullName sign /fd sha256 /tr $ts /td sha256 /f $pfx /p $env:WINDOWS_CERT_PASSWORD /d "F1R3Gaze" $f
    if ($LASTEXITCODE) { throw "signing $f failed" }
  }
  Sign "$stage\f1r3gaze.exe"; Sign "$stage\f1r3c.exe"
} else {
  Write-Warning "WINDOWS_CERT_PFX not set: the installer is unsigned"
}

$msi = Join-Path $Dist "F1R3Gaze-$Version-x64.msi"
wix build "$here\f1r3gaze.wxs" -arch x64 -d "Version=$msiVersion" -d "Bin=$stage" -d "Icons=$icons" -o $msi
if ($LASTEXITCODE) { throw "wix build failed" }
if ($signtool) { Sign $msi; Remove-Item $pfx }

# A portable zip as well.
Compress-Archive -Force -Path "$stage\f1r3gaze.exe", "$stage\f1r3c.exe" -DestinationPath (Join-Path $Dist "f1r3gaze-$Version-windows-x64.zip")
Remove-Item -Recurse -Force $stage
Write-Host "built $msi"
