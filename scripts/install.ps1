param(
  [string]$Source,
  [string]$InstallDir = (Join-Path $env:LOCALAPPDATA "Ara"),
  [string]$Mirror = "https://ghfast.top/https://github.com/srf-27/Ara/releases/latest/download"
)
$ErrorActionPreference = "Stop"
$asset = "ara-x86_64-pc-windows-msvc.tar.gz"
$tmp = Join-Path $env:TEMP ("ara-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  if ($Source -and (Test-Path $Source -PathType Container)) {
    $dir = $Source
  } else {
    if ($Source) {
      $archive = $Source
    } else {
      $archive = Join-Path $tmp $asset
      Invoke-WebRequest -Uri "$Mirror/$asset" -OutFile $archive
    }
    tar -xzf $archive -C $tmp
    if ($LASTEXITCODE -ne 0) { throw "extract failed: $archive" }
    $dir = Join-Path $tmp "ara"
  }
  $exe = Join-Path $dir "ara.exe"
  if (-not (Test-Path $exe)) { throw "ara.exe missing in $dir" }
  New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
  Copy-Item $exe (Join-Path $InstallDir "ara.exe") -Force
  $dll = Join-Path $dir "wintun.dll"
  if (Test-Path $dll) { Copy-Item $dll (Join-Path $InstallDir "wintun.dll") -Force }
  $cfg = Join-Path $InstallDir "ara.toml"
  if (-not (Test-Path $cfg)) { Copy-Item (Join-Path $dir "ara.toml.example") $cfg }
  & (Join-Path $InstallDir "ara.exe") --help | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "installed ara.exe failed --help" }
  Write-Output "installed to $InstallDir; edit ara.toml, then run as administrator: ara.exe up --config ara.toml"
} finally {
  Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
