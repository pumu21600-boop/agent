@echo off
rem P20.4: prefer the desktop shell - the built-in browser runs as a child webview inside
rem the shell (no separate Edge process). The shell keeps the service alive itself
rem (spawns pawch-agent.exe if not running). Falls back to service-only mode if the shell
rem exe is not built; the browser panel then tells the user the shell is required.
rem NOTE: keep this file ASCII-only - cmd reads .bat in the OEM codepage and UTF-8
rem Chinese turns into garbage that gets executed as a command.
cd /d "%~dp0"
if exist "desktop\target\release\agent-desktop.exe" (
  start "" "desktop\target\release\agent-desktop.exe"
  exit /b 0
)
if not exist "target\release\pawch-agent.exe" (
  echo [start] neither desktop shell nor pawch-agent.exe found. Run: cargo build --release
  pause
  exit /b 1
)
echo [start] desktop shell not built - falling back to service-only mode.
echo [start] build it with: cd desktop ^&^& cargo build --release
target\release\pawch-agent.exe start
