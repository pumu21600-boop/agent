@echo off
rem Legacy entry: forwards to pawch-agent.exe self-management.
cd /d "%~dp0"
if not exist "target\release\pawch-agent.exe" (
  echo [stop] target\release\pawch-agent.exe not found. Nothing to stop.
  pause
  exit /b 1
)
target\release\pawch-agent.exe stop
pause
