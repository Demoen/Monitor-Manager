# Monitor Manager

[![CI](https://github.com/Demoen/Monitor-Manager/actions/workflows/ci.yml/badge.svg)](https://github.com/Demoen/Monitor-Manager/actions/workflows/ci.yml)

Monitor Manager is a portable Windows tray utility that temporarily turns off selected non-primary displays while any configured application is running. It restores the previous display topology when the applications exit, automation is paused, or the utility closes.

<p align="center">
  <img src="icon.png" alt="Monitor Manager" width="200">
</p>

## Requirements

- Windows 10 or Windows 11, x64
- A local interactive desktop session
- No administrator privileges

Monitor Manager does not support Windows on ARM, 32-bit Windows, macOS, or Linux.

## Install and first run

1. Download the portable ZIP from [Releases](../../releases) and extract it to a writable location.
2. Run `MonitorManager.exe`.
3. Add one or more application executables.
4. Select the non-primary displays that should turn off.
5. Enable automation, optionally enable **Start with Windows**, and save.

There is no default application or monitor rule. On first launch, Monitor Manager opens Settings and does not change the display topology until a valid configuration is saved. Moving the executable later does not move the configuration because user data is stored in Local AppData.

Applications use OR semantics: any configured executable activates the shared display rule. Monitor Manager restores the original topology after all matching applications have been absent for three consecutive one-second scans.

Executable paths are matched case-insensitively after canonicalization. A filename-only fallback is used only when Windows denies access to a running process's full path.

## Settings

Settings shows the current controller status, configured applications, and connected displays.

- **Add Application** and **Remove Application** manage the executable paths that trigger focus mode.
- **Turn off** selects displays to remove from the temporary topology. The primary display and clone paths that share its source are protected.
- **Automation Enabled** controls whether applications can activate focus mode.
- **Start with Windows** manages the current user's startup entry. It is off by default.
- **Restore** restores the captured topology immediately and suppresses automation until all current trigger applications exit.
- **Open Logs** opens the per-user log location.
- **Save** validates and atomically stores the edited rule; **Cancel** discards unsaved edits.

A display that was saved but is currently disconnected remains visible as missing and is not substituted with another display. Reconnect it and review Settings before relying on the rule again.

## Tray menu and states

Only one instance runs for each Windows user. Launching the executable again opens the existing instance's Settings window. Startup launches use `--background` and exit silently when another instance already owns the session.

The tray menu provides:

- Current status
- **Open Settings**
- **Pause** or **Resume**
- **Restore for This Run**
- **Open Logs**
- **Exit**

Pause is persistent and restores displays before disabling automation. Restore for This Run is temporary: automation resumes only after every currently matching application has exited. Notifications are reserved for actionable failures and recovery results.

The status can be Unconfigured, Watching, Activating, Focused, Suppressed Until Clear, Paused, Restoring, Recovering, Error, or Shutting Down.

## Configuration, recovery, and logs

Per-user data is stored under:

```text
%LOCALAPPDATA%\MonitorManager\
```

- `config.json` is the versioned configuration. It stores trigger application paths, stable display device paths and last-known names, automation state, and the Start with Windows preference.
- `recovery.json` is a crash-recovery journal. It stores the captured topology and fingerprints needed to determine whether restoration is still safe.
- Rotating log files record state transitions, display API failures, recovery decisions, and configuration errors. Their total retained size is capped.

Writes use a same-directory temporary file and atomic replacement. A malformed configuration is preserved as a timestamped corrupt copy before Settings is opened. An adjacent legacy `config.json` can be imported from the executable directory; imported automation remains paused until the migrated rule is reviewed and saved.

Do not copy `recovery.json` between computers or edit it manually. It contains session-specific display identifiers.

## Display safety and crash recovery

Focus mode changes only the current display session. It does not save the focused topology to the Windows display database.

Before changing displays, Monitor Manager captures the active topology and writes a recovery journal. It validates the proposed topology, applies it, and verifies the result. A failed activation triggers an immediate rollback and leaves automation in an error state if restoration cannot be verified.

After an unexpected exit, the next launch restores only when the current topology matches the recorded focused fingerprint. If the user or Windows changed the topology in the meantime, Monitor Manager leaves it untouched and records the reason. Exact restoration is attempted first; the current Windows database topology is the fallback when session identifiers are no longer valid.

Normal Exit waits for verified restoration. If restoration fails, the app offers Retry, Cancel, or Exit Anyway. Exit Anyway retains the recovery journal for the next launch.

If the program cannot recover after a driver reset, disconnected display, or power loss, open Windows display settings or press `Win+P` to select a usable topology, then reopen Monitor Manager and review the logs.

## Limitations

- Display drivers and hardware ultimately control whether a topology request succeeds.
- Remote Desktop and locked or disconnected sessions may deny display or process information.
- Hot-plugging, docking, GPU driver updates, and clone-mode changes can invalidate session-specific identifiers. Monitor Manager will abort rather than guess.
- The primary display and every clone path sharing its source are never selectable for shutdown.
- A crash cannot restore displays until Monitor Manager runs again.
- Start with Windows is per user. There is no Windows service, installer, automatic updater, multi-profile support, or automatic primary-display switching.

## Troubleshooting

- **An application does not trigger focus mode:** open Settings and remove/re-add its current `.exe` path. Check the status and `%LOCALAPPDATA%\MonitorManager\monitor-manager.log` for access or path errors.
- **A display is shown as missing:** reconnect it, open Settings, verify its device name, and save. Monitor Manager never substitutes a different display for a missing device path.
- **Start with Windows does not run:** disable and re-enable the option, then verify the `MonitorManager` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` points to the current portable executable.
- **Recovery is blocked:** do not delete `recovery.json` until the logs have been reviewed. Restore a usable layout with Windows display settings or `Win+P`, reopen Monitor Manager, and use Retry Recovery only if the displayed topology has not been intentionally changed.
- **The tray icon disappeared after Explorer restarted:** wait a few seconds. If it does not return, launch the executable again to activate the existing instance.

## Build from source

Install Visual Studio Build Tools with the Windows SDK and MSVC x64 build tools. The repository pins Rust 1.95.0, including Clippy and rustfmt, in `rust-toolchain.toml`.

From PowerShell at the repository root:

```powershell
cargo build --manifest-path .\monitor-manager-rust\Cargo.toml --release --locked
```

The executable is written to `monitor-manager-rust\target\release\monitor-manager.exe`.

Run the same checks used for the Windows CI job:

```powershell
cargo fmt --manifest-path .\monitor-manager-rust\Cargo.toml --all -- --check
cargo clippy --manifest-path .\monitor-manager-rust\Cargo.toml --all-targets --all-features --locked -- -D warnings
cargo test --manifest-path .\monitor-manager-rust\Cargo.toml --all-targets --all-features --locked
cargo build --manifest-path .\monitor-manager-rust\Cargo.toml --release --locked
```

CI also runs RustSec advisory checks and `cargo-deny` license, source, and dependency-policy checks. `Cargo.lock` must remain committed.

## Release process

The package version in `monitor-manager-rust/Cargo.toml` and `Cargo.lock` must match the tag exactly. Pushing `v1.1.0`, for example, requires Cargo version `1.1.0`. A mismatched tag fails before compilation.

The tag workflow repeats formatting, Clippy, and tests, then creates:

- `MonitorManager-vX.Y.Z-windows-x86_64.zip`, containing the executable, README, license, and icon
- `MonitorManager-vX.Y.Z-windows-x86_64-symbols.zip`, containing the PDB separately
- `MonitorManager-vX.Y.Z-sbom.cdx.json`, a CycloneDX dependency SBOM
- `MonitorManager-vX.Y.Z-SHA256SUMS.txt`, covering all other assets

Release builds are unsigned unless both `WINDOWS_CERTIFICATE_BASE64` and `WINDOWS_CERTIFICATE_PASSWORD` repository secrets are configured. The first secret is a base64-encoded PFX; the second is its password. When both are present, the workflow signs and timestamps the executable and verifies the signature before packaging. No certificate material is placed in an artifact.

Dependabot checks Cargo crates and GitHub Actions weekly. Workflow actions are pinned to reviewed commit SHAs.

## Verify a release

Download the ZIP and checksum manifest from the same release. In PowerShell, compare the published lowercase hash with the calculated value:

```powershell
$zip = ".\MonitorManager-v1.1.0-windows-x86_64.zip"
$line = Select-String -Path .\MonitorManager-v1.1.0-SHA256SUMS.txt -Pattern "windows-x86_64\.zip$"
$expected = ($line.Line -split "\s+")[0]
$actual = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
$actual -eq $expected
```

After extraction, inspect Authenticode status when the release is expected to be signed:

```powershell
Get-AuthenticodeSignature .\MonitorManager-v1.1.0-windows-x86_64\MonitorManager.exe | Format-List Status,SignerCertificate,TimeStamperCertificate
```

`Valid` confirms a trusted signature. `NotSigned` is expected for releases built without signing secrets; the SHA-256 manifest still verifies download integrity.

## License

Monitor Manager is licensed under the [MIT License](LICENSE).
