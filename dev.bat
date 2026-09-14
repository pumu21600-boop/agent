@echo off
rem 前端热更新开发服务器：页面 http://127.0.0.1:5174（/api 与 /cancel 自动代理到 5173 主服务）
rem 改 web\src 下文件即时生效，无需构建；要更新正式 dist 时再跑 npm run build
cd /d "%~dp0web"
where node >nul 2>nul || (echo [dev] Node.js not found in PATH. Install Node.js LTS first. & pause & exit /b 1)
node node_modules\vite\bin\vite.js --host 127.0.0.1 --port 5174 --strictPort
