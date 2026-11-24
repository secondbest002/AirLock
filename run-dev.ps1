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
    # Prioritize Rust's bundled self-contained GNU linker while keeping LLVM-MinGW tools available
    $env:PATH = ($extraPaths -join ";") + ";" + $env:PATH
}

Set-Location $PSScriptRoot
npm run tauri dev
