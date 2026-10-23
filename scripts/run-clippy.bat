@echo off
rem Run cargo clippy inside the VS 2022 developer environment (same link.exe
rem hijack issue as run-tests.bat; no vswhere on this machine, path hardcoded).
rem Usage: scripts\run-clippy.bat [cargo clippy args]
setlocal
set "VCVARS=C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
if not exist "%VCVARS%" (
  echo [run-clippy] vcvars64.bat not found - update the path in this script
  exit /b 1
)
call "%VCVARS%" >nul
cd /d %~dp0..
cargo clippy --bin pawch-agent %*
