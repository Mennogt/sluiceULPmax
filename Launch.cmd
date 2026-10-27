@echo off
title sluiceULPmax
cd /d "%~dp0"
if not exist "sluiceULPmax.exe" (
  echo.
  echo   sluiceULPmax.exe not found.
  echo.
  pause
  exit /b 1
)
sluiceULPmax.exe %*
echo.
pause
