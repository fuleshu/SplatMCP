@echo off
rem Check that the app retires its bridge descriptor and writes its settings when it is
rem closed properly (not killed): a stale bridge.json would make the next start look
rem broken, and a missing settings write would lose the window position.
setlocal
for %%I in ("%~dp0..") do set "ROOT=%%~fI"
set "SPLATMCP_DATA_DIR=%ROOT%\.tmp\exit-check"
if not exist "%SPLATMCP_DATA_DIR%" mkdir "%SPLATMCP_DATA_DIR%"
del /q "%SPLATMCP_DATA_DIR%\bridge.json" "%SPLATMCP_DATA_DIR%\settings.json" 2>nul

start "" /b cmd /c "set SPLATMCP_DATA_DIR=%SPLATMCP_DATA_DIR%&& %ROOT%\target\debug\splatmcp.exe > %ROOT%\.tmp\exit-app.log 2>&1"
ping -n 7 127.0.0.1 >nul

if not exist "%SPLATMCP_DATA_DIR%\bridge.json" (
  echo FAIL: the app did not publish bridge.json on start
  goto :stop
)
echo bridge.json published on start: yes

echo === closing the window gracefully ===
powershell -NoProfile -NonInteractive -Command ^
  "$p = Get-Process splatmcp -ErrorAction SilentlyContinue | Select-Object -First 1;" ^
  "if (-not $p) { Write-Output 'app not running'; exit 1 }" ^
  "$null = $p.CloseMainWindow();" ^
  "Start-Sleep -Seconds 3;" ^
  "$still = Get-Process splatmcp -ErrorAction SilentlyContinue;" ^
  "if ($still) { Write-Output 'window close did not stop the app' } else { Write-Output 'app stopped' }"

if exist "%SPLATMCP_DATA_DIR%\bridge.json" (
  echo FAIL: bridge.json is still there after a graceful close
  type "%SPLATMCP_DATA_DIR%\bridge.json"
) else (
  echo bridge.json retired on exit: yes
)

if exist "%SPLATMCP_DATA_DIR%\settings.json" (
  echo settings.json written on exit: yes
  type "%SPLATMCP_DATA_DIR%\settings.json"
) else (
  echo settings.json written on exit: no
)

:stop
taskkill /im splatmcp.exe /f >nul 2>&1
endlocal
