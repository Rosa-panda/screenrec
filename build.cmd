@echo off
rem Build script: put WinLibs MinGW-w64 binutils on PATH, then cargo build
setlocal
set "MINGW=%LOCALAPPDATA%\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin"
set "PATH=%USERPROFILE%\.cargo\bin;%MINGW%;%PATH%"
cd /d "%~dp0"
cargo build --release %*
