@echo off
rem Build, copy to %LOCALAPPDATA%\Programs\screenrec, create a desktop shortcut
setlocal
set "MINGW=%LOCALAPPDATA%\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin"
set "PATH=%USERPROFILE%\.cargo\bin;%MINGW%;%PATH%"
cd /d "%~dp0"
if not "%1"=="--no-build" (
  cargo build --release || exit /b 1
)
set "DEST=%LOCALAPPDATA%\Programs\screenrec"
if not exist "%DEST%" mkdir "%DEST%"
copy /y "target\release\screenrec.exe" "%DEST%\" >nul
copy /y "target\release\screenrec-gui.exe" "%DEST%\" >nul
if exist "tools\rife" (
  robocopy "tools\rife" "%DEST%\rife" /e /njh /njs /ndl /nfl >nul
)
powershell -NoProfile -Command ^
  "$s = (New-Object -ComObject WScript.Shell).CreateShortcut([IO.Path]::Combine([Environment]::GetFolderPath('Desktop'), 'screenrec.lnk'));" ^
  "$s.TargetPath = '%DEST%\screenrec-gui.exe'; $s.WorkingDirectory = '%DEST%'; $s.IconLocation = '%DEST%\screenrec-gui.exe,0'; $s.Description = 'screenrec';" ^
  "$s.Save(); 'shortcut created'"
echo installed to %DEST%
