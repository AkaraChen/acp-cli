# Install the latest acpc release on Windows.
#
#   irm https://raw.githubusercontent.com/AkaraChen/acpc/main/install.ps1 | iex
#
# Environment:
#   ACPC_VERSION       release to install, e.g. v0.2.0 (default: latest)
#   ACPC_INSTALL_DIR   where to put acpc.exe (default: %LOCALAPPDATA%\Programs\acpc)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'AkaraChen/acpc'
$installDir = if ($env:ACPC_INSTALL_DIR) { $env:ACPC_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\acpc' }

$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
# ARM64 Windows runs the x64 build under emulation.
if ($arch -ne 'X64' -and $arch -ne 'Arm64') { throw "acpc-install: unsupported architecture: $arch" }
$target = 'x86_64-pc-windows-msvc'

$version = $env:ACPC_VERSION
if (-not $version) {
  $version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
}
if (-not $version.StartsWith('v')) { $version = "v$version" }

$archive = "acpc-$target.zip"
$base = "https://github.com/$repo/releases/download/$version"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("acpc-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  Write-Host "acpc-install: downloading acpc $version for $target"
  Invoke-WebRequest "$base/$archive" -OutFile "$tmp\$archive"
  Invoke-WebRequest "$base/$archive.sha256" -OutFile "$tmp\$archive.sha256"

  $expected = ((Get-Content "$tmp\$archive.sha256" -Raw).Trim() -split '\s+')[0]
  $actual = (Get-FileHash "$tmp\$archive" -Algorithm SHA256).Hash
  if ($expected -ne $actual) { throw "acpc-install: checksum mismatch for $archive" }

  Expand-Archive "$tmp\$archive" -DestinationPath "$tmp\out"
  New-Item -ItemType Directory -Force -Path $installDir | Out-Null
  Copy-Item "$tmp\out\acpc.exe" (Join-Path $installDir 'acpc.exe') -Force
  Write-Host "acpc-install: installed $installDir\acpc.exe ($version)"

  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if (($userPath -split ';') -notcontains $installDir) {
    [Environment]::SetEnvironmentVariable('Path', "$installDir;$userPath", 'User')
    Write-Host "acpc-install: added $installDir to your user PATH; open a new terminal to use acpc"
  }
} finally {
  Remove-Item -Recurse -Force $tmp
}
