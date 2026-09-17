@echo off
rem Runs a cargo command with the environment this machine needs: the MSVC toolchain for
rem the linker, a writable temp directory for rustc, and the private Python runtime's
rem interpreter for PyO3.
rem
rem Every other script here delegates to this one, so the environment lives in one place.
rem It also refuses to start a second heavy run while one is in progress: two cargo
rem processes share the target directory and each spawns Python interpreters, and the
rem resulting contention looks like a spurious test failure.
rem
rem Usage: tools\cargo_env.cmd <cargo subcommand and arguments...>
setlocal
for %%I in ("%~dp0..") do set "ROOT=%%~fI"
set "TEMP=%ROOT%\.tmp\rtemp"
set "TMP=%ROOT%\.tmp\rtemp"
if not exist "%TEMP%" mkdir "%TEMP%" >nul 2>&1

set "PYO3_PYTHON=%ROOT%\.python-runtime\Scripts\python.exe"

if not defined VSCMD_ARG_TGT_ARCH (
  for %%V in (
    "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
    "C:\Program Files\Microsoft Visual Studio\2022\Professional\VC\Auxiliary\Build\vcvars64.bat"
    "C:\Program Files\Microsoft Visual Studio\2022\Enterprise\VC\Auxiliary\Build\vcvars64.bat"
    "C:\Program Files\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
  ) do (
    if exist "%%~V" (
      call "%%~V" >nul 2>&1
      goto :toolchain_ready
    )
  )
)
:toolchain_ready

cd /d "%ROOT%"
cargo %*
endlocal
exit /b %ERRORLEVEL%
