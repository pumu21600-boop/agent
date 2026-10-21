@echo off
rem Build the release binary into target\release (the path start.bat runs).
rem Needs the VS 2022 dev environment for link.exe/cl.exe - see run-tests.bat.
rem Usage: scripts\build-release.bat
setlocal
set "VCVARS=C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
if not exist "%VCVARS%" (
  echo [build-release] vcvars64.bat not found - update the path in this script
  exit /b 1
)
call "%VCVARS%" >nul
cd /d %~dp0..
cargo build --release --bin agent
