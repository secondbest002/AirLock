param(
    [switch]$Bundle
)

$ErrorActionPreference = "Stop"

$selfContained = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-gnu\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained"
$rustBin = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-gnu\lib\rustlib\x86_64-pc-windows-gnu\bin"
$cargoBin = "$env:USERPROFILE\.cargo\bin"

$mingwBin = $null
if ($env:MINGW_BIN -and (Test-Path $env:MINGW_BIN)) {
    $mingwBin = $env:MINGW_BIN
} else {
    $wingetPkg = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\MartinStorsjo.LLVM-MinGW*\llvm-mingw*\bin" -Directory -ErrorAction SilentlyContinue |
        Sort-Object FullName -Descending |
        Select-Object -First 1
    if ($wingetPkg) {
        $mingwBin = $wingetPkg.FullName
    } else {
        $gccCmd = Get-Command "x86_64-w64-mingw32-gcc.exe" -ErrorAction SilentlyContinue
        if ($gccCmd) {
            $mingwBin = Split-Path $gccCmd.Source -Parent
        }
    }
}

$extraPaths = @($selfContained, $rustBin, $cargoBin, $mingwBin) | Where-Object { $_ -and (Test-Path $_) }
if ($extraPaths.Count -gt 0) {
    $env:PATH = ($extraPaths -join ";") + ";" + $env:PATH
}

Set-Location $PSScriptRoot

if ($Bundle) {
    npm run tauri build
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    Write-Host "Bundled installer build complete under src-tauri\target\release\bundle" -ForegroundColor Green
    exit 0
}

npm run build
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Set-Location "$PSScriptRoot\src-tauri"
cargo build --release --features custom-protocol
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$releaseDir = "$PSScriptRoot\src-tauri\target\release"
if (!(Test-Path "$releaseDir\WebView2Loader.dll")) {
    $loader = Get-ChildItem "$releaseDir\build" -Filter "WebView2Loader.dll" -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match "\\x64\\" } |
        Select-Object -First 1
    if ($loader) {
        Copy-Item $loader.FullName "$releaseDir\WebView2Loader.dll" -Force
    }
}

Write-Host "Release build complete: $releaseDir\tauri-mcp-hub.exe" -ForegroundColor Green
