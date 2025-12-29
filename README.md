# Monitor Manager

A Windows system tray application that monitors a specific .exe file and automatically disables all monitors except the primary one when the application runs, then re-enables them when it closes. Available in both Python and Rust implementations.

![Monitor Manager](icon.png)

## Features

- System tray application for easy access
- Monitor configuration management
- Lightweight and efficient
- Windows native integration

## Versions

### Python Version (`monitor-manager/`)
Built with Python and PyInstaller for a quick, portable executable.

**Build:**
```bash
cd monitor-manager
pip install -r requirements.txt
build_exe.bat
```

### Rust Version (`monitor-manager-rust/`)
Built with Rust for maximum performance and minimal resource usage.

**Build:**
```bash
cd monitor-manager-rust
cargo build --release
```

## Installation

Download the latest release from the [Releases](../../releases) page or build from source.

## CI/CD

Both versions are automatically built and tested using GitHub Actions:
- **Python Build**: Creates Windows executable via PyInstaller
- **Rust Build**: Compiles and tests with Cargo

Releases are automatically created when pushing version tags (e.g., `v1.0.0`).

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
