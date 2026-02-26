# Monitor Manager

![Rust Build](https://github.com/Demoen/lolscript/actions/workflows/rust-build.yml/badge.svg)

A Windows system tray application that monitors a specific .exe file and automatically disables all secondary monitors when the application runs, then re-enables them when it closes.

<p align="center">
  <img src="icon.png" alt="Monitor Manager" width="200">
</p>

## Features

- System tray application for easy access
- Monitor configuration management
- Lightweight and efficient
- Windows native integration

## Build

```bash
cd monitor-manager-rust
cargo build --release
```

## Installation

Download the latest release from the [Releases](../../releases) page or build from source.

## CI/CD

Automatically built and released using GitHub Actions. Releases are created when pushing version tags (e.g., `v1.0.0`).

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
