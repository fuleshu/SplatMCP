@echo off
rem Builds the SplatMCP Windows installer.
rem
rem Steps, in order:
rem   1. stage the Python runtime the app generates with, and verify it can import every
rem      pinned package from its own directory (tools\stage_python_runtime.py)
rem   2. draw the application icon from its generator (tools\make_icon.py)
rem   3. build the MCP server, which is a separate binary the installer also ships
rem   4. package the app, the MCP server and the runtime into an NSIS installer
rem
rem The result is target\release\bundle\nsis\SplatMCP_<version>_x64-setup.exe.
rem
rem Usage: tools\build_installer.cmd
setlocal enabledelayedexpansion
for %%I in ("%~dp0..") do set "ROOT=%%~fI"

echo === 1/4 stage the Python runtime ===
python "%~dp0stage_python_runtime.py"
if errorlevel 1 (
  echo build_installer: staging the Python runtime failed
  exit /b 1
)

echo.
echo === 2/4 application icon ===
"%ROOT%\.python-runtime\Scripts\python.exe" "%~dp0make_icon.py"
if errorlevel 1 (
  echo build_installer: drawing the icon failed
  exit /b 1
)

echo.
echo === 3/4 build the MCP server ===
rem The MCP client launches this binary itself, so the installer ships a copy next to the
rem app. `tauri build` only builds the app, which is why this step comes first and the
rem result is copied into the resource directory the bundle configuration maps.
call "%~dp0cargo_env.cmd" build --release -p splatmcp-mcp
if errorlevel 1 (
  echo build_installer: building the MCP server failed
  exit /b 1
)
if not exist "%ROOT%\src-tauri\resources\mcp" mkdir "%ROOT%\src-tauri\resources\mcp"
copy /y "%ROOT%\target\release\splatmcp-mcp.exe" "%ROOT%\src-tauri\resources\mcp\splatmcp-mcp.exe" >nul
if errorlevel 1 (
  echo build_installer: could not stage the MCP server
  exit /b 1
)
echo    staged resources\mcp\splatmcp-mcp.exe

echo.
echo === 4/4 package the installer ===
rem `tauri build` compiles the app in release mode and bundles it. It needs the same
rem environment as cargo: the MSVC linker and the Python interpreter PyO3 links against.
call "%~dp0cargo_env.cmd" tauri build
if errorlevel 1 (
  echo build_installer: packaging failed
  exit /b 1
)

echo.
echo === installer ===
dir /b "%ROOT%\target\release\bundle\nsis\*.exe" 2>nul
echo.
echo Install it, or verify it silently with tools\installer_check.cmd.
endlocal
