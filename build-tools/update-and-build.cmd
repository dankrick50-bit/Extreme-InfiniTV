@echo off
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0update-and-build.ps1" %*
echo.
pause
