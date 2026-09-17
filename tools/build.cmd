@echo off
rem Builds the workspace. See tools\cargo_env.cmd for the environment it sets up.
rem
rem Usage: tools\build.cmd [cargo build arguments]
call "%~dp0cargo_env.cmd" build %*
exit /b %ERRORLEVEL%
