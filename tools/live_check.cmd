@echo off
rem Live check of the SplatMCP bridge: starts the app, drives it with the scripted
rem client, prints the results and stops the app again.
rem
rem Usage: tools\live_check.cmd [ply-file]
setlocal enabledelayedexpansion
for %%I in ("%~dp0..") do set "ROOT=%%~fI"
set "SPLATMCP_DATA_DIR=%ROOT%\.tmp\appdata"
if not exist "%SPLATMCP_DATA_DIR%" mkdir "%SPLATMCP_DATA_DIR%"
set "PLY=%~1"
if "%PLY%"=="" set "PLY=%ROOT%\crates\splatmcp-core\tests\data\external_grid.ply"

del /q "%SPLATMCP_DATA_DIR%\bridge.json" 2>nul
start "" /b cmd /c "set SPLATMCP_DATA_DIR=%SPLATMCP_DATA_DIR%&& %ROOT%\target\debug\splatmcp.exe > %ROOT%\.tmp\app.log 2>&1"
ping -n 7 127.0.0.1 >nul

echo === app log ===
type "%ROOT%\.tmp\app.log"
echo === ping ===
python "%ROOT%\tools\bridge_client.py" ping
echo === status before load ===
python "%ROOT%\tools\bridge_client.py" status
echo === load %PLY% ===
python "%ROOT%\tools\bridge_client.py" load-ply "%PLY%"
echo === status after load ===
python "%ROOT%\tools\bridge_client.py" status
echo === set camera fit ===
python "%ROOT%\tools\bridge_client.py" set-camera --fit
echo === camera orbit ===
python "%ROOT%\tools\bridge_client.py" set-camera --azimuth 35 --elevation 18 --distance 4
echo === screenshot ===
python "%ROOT%\tools\bridge_client.py" screenshot --width 800 --out "%ROOT%\.tmp\shot.png"
echo === stopping app ===
taskkill /im splatmcp.exe /f >nul 2>&1
endlocal
