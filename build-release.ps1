param(
    [switch]$Bundle,
    [switch]$Portable
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

if ($Portable) {
    $tauriConf = Get-Content "$PSScriptRoot\src-tauri\tauri.conf.json" -Raw | ConvertFrom-Json
    $version = $tauriConf.version
    $portableRoot = "$releaseDir\portable\Airlock-Portable"
    $zipPath = "$releaseDir\Airlock-v$version-windows-x64-portable.zip"

    if (Test-Path $portableRoot) {
        Remove-Item $portableRoot -Recurse -Force
    }
    New-Item -ItemType Directory -Path "$portableRoot\airlock-data" -Force | Out-Null
    Copy-Item "$releaseDir\tauri-mcp-hub.exe" "$portableRoot\tauri-mcp-hub.exe" -Force
    if (Test-Path "$releaseDir\WebView2Loader.dll") {
        Copy-Item "$releaseDir\WebView2Loader.dll" "$portableRoot\WebView2Loader.dll" -Force
    }
    Set-Content -Path "$portableRoot\portable.flag" -Value "Airlock True Portable Mode marker. Keep this file next to tauri-mcp-hub.exe." -Encoding UTF8
    @"
Airlock v$version — True Portable Edition (Windows x64)
=======================================================

1. Jalankan `tauri-mcp-hub.exe` langsung dari folder ini (tanpa instalasi).
2. Karena file `portable.flag` ada di sebelah `.exe`, seluruh data aplikasi
   (config.json, auth.json, secrets.dpapi, snapshots, checkpoints, chat history,
   dan cache WebView2) disimpan di dalam folder `airlock-data\`.
3. Anda juga dapat menjalankan dengan flag `--portable` atau mengatur environment
   variable `AIRLOCK_DATA_DIR` untuk menentukan lokasi folder data khusus.
4. Catatan Keamanan (Windows DPAPI): `secrets.dpapi` dienkripsi memakai Windows
   DPAPI yang terikat ke akun Windows pada PC saat ini. Jika folder portable
   dipindahkan ke PC lain, cukup masukkan ulang API key / token di PC baru.
"@ | Set-Content -Path "$portableRoot\PORTABLE-README.txt" -Encoding UTF8

    if (Test-Path $zipPath) {
        Remove-Item $zipPath -Force
    }
    Compress-Archive -Path "$portableRoot\*" -DestinationPath $zipPath -Force
    Write-Host "Portable package complete: $zipPath" -ForegroundColor Green
}
