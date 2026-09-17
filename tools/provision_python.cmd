@echo off
rem Assembles the application private CPython runtime SplatMCP generates with.
rem
rem The runtime lives in .python-runtime at the repository root and holds the pinned
rem packages the generation recipes need. It is deliberately NOT the system Python: the app
rem must work on a machine whose PATH, Conda or system interpreter is anything at all.
rem
rem Usage: tools\provision_python.cmd [target-dir]
rem After it succeeds, `cargo test -p splatmcp-python` finds the runtime on its own
rem (crates\splatmcp-python\build.rs), and the app resolves it through
rem SPLATMCP_PYTHON_HOME or <app data>\python-runtime.
setlocal
set "PYTHON_VERSION=3.13"
set "NUMPY_VERSION=2.3.3"
set "SCIPY_VERSION=1.16.2"
set "PILLOW_VERSION=11.3.0"

for %%I in ("%~dp0..") do set "ROOT=%%~fI"
set "TARGET=%ROOT%\.python-runtime"
if not "%~1"=="" set "TARGET=%~f1"

where python >nul 2>nul
if errorlevel 1 (
  echo python was not found on PATH; install CPython %PYTHON_VERSION% x64 first.
  exit /b 1
)

echo === creating %TARGET% ===
if not exist "%TARGET%" (
  python -m venv "%TARGET%" || exit /b 1
)

set "PY=%TARGET%\Scripts\python.exe"
if not exist "%PY%" (
  echo the runtime at %TARGET% has no interpreter; delete it and run this script again.
  exit /b 1
)

echo === installing pinned packages ===
"%PY%" -m pip install --disable-pip-version-check --quiet --upgrade pip || exit /b 1
"%PY%" -m pip install --disable-pip-version-check --quiet --only-binary :all: ^
  "numpy==%NUMPY_VERSION%" "scipy==%SCIPY_VERSION%" "pillow==%PILLOW_VERSION%" || exit /b 1

echo === recording runtime-manifest.json ===
rem The manifest records the interpreter's own sys.path with the user's site-packages
rem removed. The embedded interpreter installs exactly that list, which is what keeps a
rem generation job from importing a package the app never shipped. The writer explains it.
"%PY%" "%~dp0write_runtime_manifest.py" "%TARGET%" || exit /b 1

echo.
echo The app installs this runtime's own sys.path and drops everything else, so a recipe
echo imports the pinned packages only. Point the app at it with:
echo   set SPLATMCP_PYTHON_HOME=%TARGET%
echo.
echo runtime ready: %TARGET%
echo point the desktop app at it with:  set SPLATMCP_PYTHON_HOME=%TARGET%
endlocal
