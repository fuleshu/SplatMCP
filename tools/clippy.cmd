@echo off
rem Runs clippy. See tools\cargo_env.cmd for the environment it sets up.
rem
rem The pre-existing crates (splatmcp-core, and the MCP crate's app_launch) carry warnings
rem that predate the Python work and are not addressed here; pass -p to focus on one crate.
rem
rem Usage: tools\clippy.cmd [cargo clippy arguments]
call "%~dp0cargo_env.cmd" clippy %*
exit /b %ERRORLEVEL%
