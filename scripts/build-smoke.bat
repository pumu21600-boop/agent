@echo off
rem Build the release smoke binary into target-smoke (keeps the user's target/ dir
rem and the running service's exe untouched). Needs the VS 2022 dev environment
rem for link.exe/cl.exe - see run-tests.bat for why.
rem Usage: scripts\build-smoke.bat
setlocal
set "VCVARS=C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
if not exist "%VCVARS%" (
  echo [build-smoke] vcvars64.bat not found - update the path in this script
  exit /b 1
)
call "%VCVARS%" >nul
cd /d %~dp0..
set CARGO_TARGET_DIR=target-smoke
cargo build --release --bin pawch-agent
