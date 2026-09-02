@echo off
rem Start Agent in background (detached from this terminal).
rem Double-click to run. Use stop.bat to stop.
cd /d "%~dp0"
if not exist "target\release\agent.exe" (
  echo [start] target\release\agent.exe not found. Run: cargo build --release
  pause
  exit /b 1
)
tasklist /FI "IMAGENAME eq agent.exe" 2>nul | findstr /I "agent.exe" >nul
if %errorlevel%==0 (
  echo [start] Agent is already running: http://127.0.0.1:8321
  pause
  exit /b 0
)
powershell -NoProfile -Command "Start-Process -FilePath '%~dp0target\release\agent.exe' -WorkingDirectory '%~dp0' -WindowStyle Hidden"
echo [start] Agent started in background: http://127.0.0.1:8321  (stop: stop.bat)
