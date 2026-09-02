@echo off
rem Stop the background Agent process.
taskkill /F /IM agent.exe >nul 2>&1
if %errorlevel%==0 (
  echo [stop] Agent stopped.
) else (
  echo [stop] No running agent process.
)
pause
