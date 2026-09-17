@echo off
rem Runs the workspace test suite. See tools\cargo_env.cmd for the environment it sets up.
rem
rem The full cargo transcript goes to .tmp\workspace_tests.log and only a compact summary is
rem printed. A harness that captures this command's output into a pipe would otherwise have
rem to drain the whole transcript while the process runs, and an undrained pipe can stall
rem the build - which looks exactly like a hung test.
rem
rem Usage: tools\test_workspace.cmd [cargo test arguments]
setlocal enabledelayedexpansion
for %%I in ("%~dp0..") do set "ROOT=%%~fI"

set "LOG=%ROOT%\.tmp\workspace_tests.log"
if not exist "%ROOT%\.tmp" mkdir "%ROOT%\.tmp" >nul 2>&1
if exist "%LOG%" del /q "%LOG%" >nul 2>&1

call "%~dp0cargo_env.cmd" test %* > "%LOG%" 2>&1
set "STATUS=%ERRORLEVEL%"

echo --- test result lines ---
findstr /c:"test result" "%LOG%"
echo --- failures, if any ---
findstr /c:"FAILED" /c:"panicked at" /c:"error[" "%LOG%"
echo full transcript: %LOG%

if not "%STATUS%"=="0" (
  echo test_workspace: FAILED with exit code %STATUS%
  endlocal
  exit /b %STATUS%
)

echo test_workspace: ok
endlocal
exit /b 0
