@echo off
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0get-installers.ps1" %*
echo.
pause
