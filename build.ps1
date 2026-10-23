# Builds FileListCinema.exe (Rust, release) and gives it the app icon and file details.
# Usage: powershell -ExecutionPolicy Bypass -File build.ps1
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
# Build output on the SSD: compiling on the (busy) download hard drive is very slow.
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = Join-Path $env:LOCALAPPDATA 'flc-target' }
# Rust (rustup, GNU toolchain) + MinGW-w64 binutils (dlltool, needed by the windows-sys crates) from WinLibs
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:LOCALAPPDATA\flc-tools\mingw64\bin;" + $env:Path

Write-Host '> cargo build --release'
cargo build --release --manifest-path (Join-Path $root 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

$built = Join-Path $env:CARGO_TARGET_DIR 'release\FileListCinema.exe'
$staged = Join-Path $env:CARGO_TARGET_DIR 'release\FileListCinema-stamped.exe'
Copy-Item $built $staged -Force

$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
& (Join-Path $root 'tools\rcedit-x64.exe') $staged `
  --set-icon (Join-Path $root 'assets\icon.ico') `
  --set-file-version $version --set-product-version $version `
  --set-version-string ProductName 'FileList Cinema' `
  --set-version-string FileDescription 'FileList Cinema' `
  --set-version-string OriginalFilename 'FileListCinema.exe' `
  --set-version-string InternalName 'FileListCinema'
if ($LASTEXITCODE -ne 0) { throw 'rcedit failed' }

$out = Join-Path $root 'FileListCinema.exe'
try {
  Move-Item $staged $out -Force
} catch {
  throw "FileListCinema.exe is running, so Windows won't let it be replaced. Close it and build again."
}
'{0} ({1:N1} MB)' -f $out, ((Get-Item $out).Length / 1MB)
