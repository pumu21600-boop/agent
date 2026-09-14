@echo off
rem Legacy entry: forwards to agent.exe self-management (start = background + open browser).
cd /d "%~dp0"
if not exist "target\release\agent.exe" (
  echo [start] target\release\agent.exe not found. Run: cargo build --release
  pause
  exit /b 1
)
target\release\agent.exe start
