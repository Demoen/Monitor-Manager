@echo off
echo Building Monitor Manager (Rust)...
echo.

cargo build --release

if %errorlevel% equ 0 (
    echo.
    echo ========================================
    echo Build successful!
    echo ========================================
    echo.
    echo Executable location: target\release\monitor-manager.exe
    echo.
    dir target\release\monitor-manager.exe | findstr /C:"monitor-manager.exe"
    echo.
    echo To run: .\target\release\monitor-manager.exe
    echo.
) else (
    echo.
    echo Build failed! Make sure Rust is installed.
    echo Visit: https://rustup.rs/
    echo.
)

pause
