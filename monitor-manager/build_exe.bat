@echo off
echo Cleaning up previous builds...
if exist build rmdir /s /q build
if exist dist rmdir /s /q dist

echo.
echo Installing required packages...
pip install -r requirements.txt
pip install pyinstaller

echo.
echo Generating icon...
python create_icon.py

echo.
echo Building executable...
:: Use the spec file that has the icon properly configured
pyinstaller --clean MonitorManager.spec

echo.
echo Done! The executable is in the 'dist' folder.
echo NOTE: We renamed it to 'LoL_Monitor_Tool.exe' to force Windows to see the new icon.
echo You can run "LoL_Monitor_Manager.exe" from there.
pause
