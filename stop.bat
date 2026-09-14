@echo off
rem Legacy entry: forwards to agent.exe self-management.
cd /d "%~dp0"
if not exist "target\release\agent.exe" (
  echo [stop] target\release\agent.exe not found. Nothing to stop.
  pause
  exit /b 1
)
target\release\agent.exe stop
pause
