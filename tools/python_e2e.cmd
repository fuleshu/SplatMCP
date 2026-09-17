@echo off
rem Builds and then runs the live end-to-end check of embedded Python generation.
rem
rem The check itself is python tools\mcp_python_e2e.py: one real MCP stdio session driving
rem the real desktop app, which hosts the single generation service and the embedded
rem interpreter. This wrapper exists so the Adashi QA job can run it with one command,
rem whatever defaults a shell brings.
setlocal
for %%I in ("%~dp0..") do set "ROOT=%%~fI"

echo === building the app and the MCP server ===
call "%~dp0build.cmd" -p splatmcp -p splatmcp-mcp
if errorlevel 1 (
  echo python_e2e: the build failed
  exit /b 1
)

echo.
echo === live end-to-end check over the real MCP path ===
rem `-u` so each check is written as it happens: a run that is still going when a harness
rem stops reading should show how far it got, not an empty log.
python -u "%~dp0mcp_python_e2e.py"
if errorlevel 1 (
  echo python_e2e: FAILED
  exit /b 1
)

echo python_e2e: ok
endlocal
exit /b 0
