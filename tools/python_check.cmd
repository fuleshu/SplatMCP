@echo off
rem Builds and runs the embedded-Python generation tests.
rem
rem This is the QA entry point for the Python half of the milestone. It refuses to run
rem quietly without a runtime, because a "green" run that skipped every test would be worse
rem than a red one.
rem
rem The test output goes to .tmp\python_check.log and only a compact summary is printed. A
rem harness that captures this command's output into a pipe would otherwise have to drain
rem the whole cargo transcript while the process runs, and an undrained pipe can stall the
rem build.
setlocal enabledelayedexpansion
for %%I in ("%~dp0..") do set "ROOT=%%~fI"

set "LOG=%ROOT%\.tmp\python_check.log"
if not exist "%ROOT%\.tmp" mkdir "%ROOT%\.tmp" >nul 2>&1
if exist "%LOG%" del /q "%LOG%" >nul 2>&1

set "PY=%ROOT%\.python-runtime\Scripts\python.exe"
if not exist "%PY%" (
  echo python_check: no private runtime at %ROOT%\.python-runtime
  echo python_check: run tools\provision_python.cmd first
  exit /b 1
)

echo === private runtime ===
"%PY%" -c "import numpy, scipy, PIL, sys; print('python', sys.version.split()[0], '| numpy', numpy.__version__, '| scipy', scipy.__version__, '| pillow', PIL.__version__)" > "%LOG%" 2>&1
type "%LOG%"
if errorlevel 1 (
  echo python_check: the private runtime cannot import its pinned packages
  exit /b 1
)

echo.
echo === crate unit tests and embedded generation tests ===
call "%~dp0cargo_env.cmd" test -p splatmcp-python >> "%LOG%" 2>&1
set "STATUS=%ERRORLEVEL%"

echo --- test result lines ---
findstr /c:"test result" "%LOG%"
echo --- failures, if any ---
findstr /c:"FAILED" /c:"panicked at" /c:"error[" "%LOG%"
echo full transcript: %LOG%

if not "%STATUS%"=="0" (
  echo python_check: FAILED with exit code %STATUS%
  endlocal
  exit /b %STATUS%
)

echo python_check: ok
endlocal
exit /b 0
