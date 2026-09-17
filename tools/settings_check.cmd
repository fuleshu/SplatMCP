@echo off
rem Live check of settings.json window persistence.
rem
rem Starts the app with an isolated data directory, moves and resizes the window through
rem the Win32 API (no mouse needed), closes the app, prints the stored settings and
rem restarts it to confirm the geometry is restored.
setlocal enabledelayedexpansion
for %%I in ("%~dp0..") do set "ROOT=%%~fI"
set "SPLATMCP_DATA_DIR=%ROOT%\.tmp\settings-check"
if not exist "%SPLATMCP_DATA_DIR%" mkdir "%SPLATMCP_DATA_DIR%"
del /q "%SPLATMCP_DATA_DIR%\settings.json" 2>nul

echo === first run: default geometry ===
start "" /b cmd /c "set SPLATMCP_DATA_DIR=%SPLATMCP_DATA_DIR%&& %ROOT%\target\debug\splatmcp.exe > %ROOT%\.tmp\settings-app.log 2>&1"
ping -n 7 127.0.0.1 >nul
powershell -NoProfile -NonInteractive -Command ^
  "$p = Get-Process splatmcp -ErrorAction SilentlyContinue | Select-Object -First 1;" ^
  "if (-not $p) { Write-Output 'app not running'; exit 1 }" ^
  "Add-Type -Namespace Win -Name Api -MemberDefinition '[DllImport(\"user32.dll\")] public static extern bool MoveWindow(IntPtr h, int x, int y, int w, int hh, bool rp);';" ^
  "$h = $p.MainWindowHandle;" ^
  "Write-Output ('before: ' + $p.MainWindowTitle);" ^
  "[Win.Api]::MoveWindow($h, 140, 90, 900, 640, $true) | Out-Null;" ^
  "Start-Sleep -Milliseconds 900;" ^
  "Write-Output 'moved to 140,90 900x640'"

echo === closing the app ===
taskkill /im splatmcp.exe /f >nul 2>&1
ping -n 3 127.0.0.1 >nul
echo === stored settings ===
type "%SPLATMCP_DATA_DIR%\settings.json"

echo === second run: geometry should be restored ===
start "" /b cmd /c "set SPLATMCP_DATA_DIR=%SPLATMCP_DATA_DIR%&& %ROOT%\target\debug\splatmcp.exe > %ROOT%\.tmp\settings-app.log 2>&1"
ping -n 7 127.0.0.1 >nul
powershell -NoProfile -NonInteractive -Command ^
  "$p = Get-Process splatmcp -ErrorAction SilentlyContinue | Select-Object -First 1;" ^
  "if (-not $p) { Write-Output 'app not running'; exit 1 }" ^
  "Add-Type -Namespace Win -Name Api2 -MemberDefinition '[DllImport(\"user32.dll\")] public static extern bool GetWindowRect(IntPtr h, out RECT r); public struct RECT { public int Left, Top, Right, Bottom; }';" ^
  "$r = New-Object Win.Api2+RECT;" ^
  "[Win.Api2]::GetWindowRect($p.MainWindowHandle, [ref]$r) | Out-Null;" ^
  "Write-Output ('restored outer rect: x=' + $r.Left + ' y=' + $r.Top + ' w=' + ($r.Right - $r.Left) + ' h=' + ($r.Bottom - $r.Top))"

taskkill /im splatmcp.exe /f >nul 2>&1
endlocal
