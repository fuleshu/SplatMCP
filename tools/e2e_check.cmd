@echo off
rem Full end-to-end check of SplatMCP: one scripted MCP session that creates, inspects,
rem edits, loads, captures, saves and reloads a splat through the real desktop app.
rem
rem Every step prints its tool reply, so a failure is visible in the output rather than
rem inferred. The frames it writes are in .tmp\ for visual inspection.
setlocal enabledelayedexpansion
for %%I in ("%~dp0..") do set "ROOT=%%~fI"
set "SPLATMCP_DATA_DIR=%ROOT%\.tmp\e2e"
set "OUT=%ROOT%\.tmp"
if not exist "%SPLATMCP_DATA_DIR%" mkdir "%SPLATMCP_DATA_DIR%"
del /q "%SPLATMCP_DATA_DIR%\bridge.json" 2>nul
del /q "%OUT%\e2e-*.ply" "%OUT%\e2e-*.png" "%OUT%\e2e-*.jpg" 2>nul

echo === starting the desktop app ===
start "" /b cmd /c "set SPLATMCP_DATA_DIR=%SPLATMCP_DATA_DIR%&& %ROOT%\target\debug\splatmcp.exe > %ROOT%\.tmp\e2e-app.log 2>&1"
ping -n 7 127.0.0.1 >nul
type "%ROOT%\.tmp\e2e-app.log"

echo.
echo === 1. status before anything is displayed ===
python "%ROOT%\tools\mcp_session.py" --data-dir "%SPLATMCP_DATA_DIR%" --call splatmcp_status "{}"

echo.
echo === 2. create a splat, write it and display it ===
python "%ROOT%\tools\mcp_session.py" --data-dir "%SPLATMCP_DATA_DIR%" ^
  --call create_splat "{\"shape\":\"sphere\",\"count\":6000,\"size\":1.2,\"jitter\":0.06,\"color\":[0.25,0.6,0.9],\"radius\":0.025,\"color_variation\":0.08,\"seed\":3,\"path\":\".tmp/e2e-created.ply\"}"

echo.
echo === 3. inspect what is displayed ===
python "%ROOT%\tools\mcp_session.py" --data-dir "%SPLATMCP_DATA_DIR%" --call splat_info "{\"points\":2}"

echo.
echo === 4. edit it: recolour the top half, duplicate it downwards, halve the original opacity ===
python "%ROOT%\tools\mcp_session.py" --data-dir "%SPLATMCP_DATA_DIR%" ^
  --call edit_splat "{\"ops\":[{\"op\":\"set_color\",\"color\":[0.95,0.35,0.2],\"within\":[-2,0.2,-2,2,2,2]},{\"op\":\"duplicate\",\"by\":[0,-1.6,0]},{\"op\":\"set_opacity\",\"factor\":0.5,\"first\":6000},{\"op\":\"set_radius\",\"factor\":1.4}],\"path\":\".tmp/e2e-edited.ply\"}"

echo.
echo === 5. frame the result and capture it ===
python "%ROOT%\tools\mcp_session.py" --data-dir "%SPLATMCP_DATA_DIR%" ^
  --call set_camera "{\"fit\":true}" --call get_screenshot "{\"width\":640}" ^
  --out-dir "%OUT%" --prefix e2e-step5-

echo.
echo === 6. orbit the camera and capture a second frame as JPEG ===
python "%ROOT%\tools\mcp_session.py" --data-dir "%SPLATMCP_DATA_DIR%" ^
  --call set_camera "{\"azimuth\":140,\"elevation\":25,\"distance\":5}" --call get_camera "{}" ^
  --call get_screenshot "{\"width\":480,\"format\":\"jpeg\",\"quality\":85}" ^
  --out-dir "%OUT%" --prefix e2e-step6-

echo.
echo === 7. reload the saved file from disk and confirm the point count ===
python "%ROOT%\tools\mcp_session.py" --data-dir "%SPLATMCP_DATA_DIR%" ^
  --call load_splat "{\"path\":\".tmp/e2e-edited.ply\"}" --call splat_info "{\"source\":\".tmp/e2e-edited.ply\"}"

echo.
echo === 8. edit a file (not the window) and confirm the window is untouched ===
python "%ROOT%\tools\mcp_session.py" --data-dir "%SPLATMCP_DATA_DIR%" ^
  --call edit_splat "{\"source\":\".tmp/e2e-created.ply\",\"ops\":[{\"op\":\"scale\",\"factor\":0.5}],\"display\":false}" ^
  --call splat_info "{\"source\":\"viewer\"}"

echo.
echo === stopping the app (forced, so a stale descriptor is expected here) ===
taskkill /im splatmcp.exe /f >nul 2>&1
ping -n 3 127.0.0.1 >nul
if exist "%SPLATMCP_DATA_DIR%\bridge.json" (
  echo bridge.json is still present: expected after a forced kill, and the MCP server
  echo treats such a descriptor as stale and starts a fresh app. tools\exit_check.cmd
  echo covers the graceful close, which does retire it.
) else (
  echo bridge.json was retired on exit
)
type "%ROOT%\.tmp\e2e-app.log"
echo.
echo === produced files ===
dir /b "%OUT%\e2e-*" 2>nul
echo.
echo === next: the captured frames are in .tmp; tools\exit_check.cmd checks a clean exit ===
endlocal
