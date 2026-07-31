use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use thiserror::Error;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    RRF_RT_REG_SZ,
};

use crate::config::{ConfigError, ConfigStore, ConfigV2};

pub const RUN_VALUE_NAME: &str = "MonitorManager";

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub trait StartupBackend: Send + Sync {
    fn read_command(&self) -> io::Result<Option<OsString>>;
    fn write_command(&self, command: Option<&OsStr>) -> io::Result<()>;
}

#[derive(Default)]
pub struct RegistryStartup;

impl RegistryStartup {
    pub fn new() -> Self {
        Self
    }
}

impl StartupBackend for RegistryStartup {
    fn read_command(&self) -> io::Result<Option<OsString>> {
        let Some(key) = open_run_key()? else {
            return Ok(None);
        };
        let value_name = wide_null(OsStr::new(RUN_VALUE_NAME));

        for _ in 0..4 {
            let mut byte_count = 0u32;
            let status = unsafe {
                RegGetValueW(
                    key.0,
                    PCWSTR::null(),
                    PCWSTR(value_name.as_ptr()),
                    RRF_RT_REG_SZ,
                    None,
                    None,
                    Some(&mut byte_count),
                )
            };
            if is_missing(status) {
                return Ok(None);
            }
            check_status(status)?;

            let mut value = vec![0u16; (byte_count as usize).div_ceil(2).max(1)];
            let status = unsafe {
                RegGetValueW(
                    key.0,
                    PCWSTR::null(),
                    PCWSTR(value_name.as_ptr()),
                    RRF_RT_REG_SZ,
                    None,
                    Some(value.as_mut_ptr().cast()),
                    Some(&mut byte_count),
                )
            };
            if status == ERROR_MORE_DATA {
                continue;
            }
            check_status(status)?;
            let length = (byte_count as usize / 2).min(value.len());
            value.truncate(length);
            if value.last() == Some(&0) {
                value.pop();
            }
            return Ok(Some(OsString::from_wide(&value)));
        }

        Err(io::Error::other(
            "startup registry value changed repeatedly while reading",
        ))
    }

    fn write_command(&self, command: Option<&OsStr>) -> io::Result<()> {
        let value_name = wide_null(OsStr::new(RUN_VALUE_NAME));
        match command {
            Some(command) => {
                let key = create_run_key()?;
                let value = wide_null(command);
                let bytes = unsafe {
                    std::slice::from_raw_parts(value.as_ptr().cast::<u8>(), value.len() * 2)
                };
                check_status(unsafe {
                    RegSetValueExW(key.0, PCWSTR(value_name.as_ptr()), 0, REG_SZ, Some(bytes))
                })
            }
            None => {
                let Some(key) = open_run_key_for_write()? else {
                    return Ok(());
                };
                let status = unsafe { RegDeleteValueW(key.0, PCWSTR(value_name.as_ptr())) };
                if is_missing(status) {
                    Ok(())
                } else {
                    check_status(status)
                }
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum SettingsSaveError {
    #[error("could not build the startup command: {0}")]
    StartupCommand(#[source] io::Error),
    #[error("could not read the current startup registration: {0}")]
    StartupRead(#[source] io::Error),
    #[error("could not update the startup registration: {0}")]
    StartupWrite(#[source] io::Error),
    #[error("could not save configuration: {source}; startup rollback error: {rollback_error:?}")]
    Config {
        #[source]
        source: ConfigError,
        rollback_error: Option<io::Error>,
    },
}

pub fn startup_command(executable: &Path) -> io::Result<OsString> {
    if !executable.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "startup executable path must be absolute",
        ));
    }
    if executable.as_os_str().to_string_lossy().contains('"') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "startup executable path contains a quote",
        ));
    }

    let mut command = OsString::from("\"");
    command.push(executable.as_os_str());
    command.push("\" --background");
    Ok(command)
}

pub fn save_config_transactionally(
    store: &ConfigStore,
    startup: &dyn StartupBackend,
    executable: &Path,
    next_config: &ConfigV2,
) -> Result<(), SettingsSaveError> {
    save_config_with_mode(store, startup, executable, next_config, false)
}

pub fn save_persisted_config_transactionally(
    store: &ConfigStore,
    startup: &dyn StartupBackend,
    executable: &Path,
    next_config: &ConfigV2,
) -> Result<(), SettingsSaveError> {
    save_config_with_mode(store, startup, executable, next_config, true)
}

fn save_config_with_mode(
    store: &ConfigStore,
    startup: &dyn StartupBackend,
    executable: &Path,
    next_config: &ConfigV2,
    persisted: bool,
) -> Result<(), SettingsSaveError> {
    let validation = if persisted {
        next_config.validate_persisted()
    } else {
        next_config.validate()
    };
    validation
        .map_err(ConfigError::from)
        .map_err(|source| SettingsSaveError::Config {
            source,
            rollback_error: None,
        })?;
    let desired = next_config
        .start_with_windows
        .then(|| startup_command(executable))
        .transpose()
        .map_err(SettingsSaveError::StartupCommand)?;
    let previous = startup
        .read_command()
        .map_err(SettingsSaveError::StartupRead)?;
    let changed = previous != desired;
    if changed {
        startup
            .write_command(desired.as_deref())
            .map_err(SettingsSaveError::StartupWrite)?;
    }

    let save_result = if persisted {
        store.save_persisted(next_config)
    } else {
        store.save(next_config)
    };
    match save_result {
        Ok(()) => Ok(()),
        Err(source) => {
            let rollback_error = if changed {
                startup.write_command(previous.as_deref()).err()
            } else {
                None
            };
            Err(SettingsSaveError::Config {
                source,
                rollback_error,
            })
        }
    }
}

struct OwnedKey(HKEY);

impl Drop for OwnedKey {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

fn open_run_key() -> io::Result<Option<OwnedKey>> {
    open_existing_run_key(KEY_QUERY_VALUE)
}

fn open_run_key_for_write() -> io::Result<Option<OwnedKey>> {
    open_existing_run_key(KEY_SET_VALUE)
}

fn open_existing_run_key(
    access: windows::Win32::System::Registry::REG_SAM_FLAGS,
) -> io::Result<Option<OwnedKey>> {
    let key_name = wide_null(OsStr::new(RUN_KEY));
    let mut key = HKEY::default();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_name.as_ptr()),
            0,
            access,
            &mut key,
        )
    };
    if is_missing(status) {
        return Ok(None);
    }
    check_status(status)?;
    Ok(Some(OwnedKey(key)))
}

fn create_run_key() -> io::Result<OwnedKey> {
    let key_name = wide_null(OsStr::new(RUN_KEY));
    let mut key = HKEY::default();
    check_status(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_name.as_ptr()),
            0,
            PWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    })?;
    Ok(OwnedKey(key))
}

fn is_missing(status: WIN32_ERROR) -> bool {
    status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND
}

fn check_status(status: WIN32_ERROR) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status.0 as i32))
    }
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DisabledDisplay;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Default)]
    struct MockStartup {
        command: Mutex<Option<OsString>>,
    }

    impl StartupBackend for MockStartup {
        fn read_command(&self) -> io::Result<Option<OsString>> {
            Ok(self.command.lock().unwrap().clone())
        }

        fn write_command(&self, command: Option<&OsStr>) -> io::Result<()> {
            *self.command.lock().unwrap() = command.map(OsString::from);
            Ok(())
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "monitor-manager-startup-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn valid_config(directory: &Path) -> ConfigV2 {
        let trigger = directory.join("game.exe");
        fs::write(&trigger, b"exe").unwrap();
        ConfigV2 {
            trigger_apps: vec![trigger],
            disabled_displays: vec![DisabledDisplay {
                device_path: "display-2".to_owned(),
                last_known_name: "Second display".to_owned(),
            }],
            start_with_windows: true,
            ..ConfigV2::default()
        }
    }

    #[test]
    fn startup_command_always_quotes_the_executable() {
        let command = startup_command(Path::new(r"C:\Program Files\Monitor Manager.exe")).unwrap();
        assert_eq!(
            command,
            OsString::from(r#""C:\Program Files\Monitor Manager.exe" --background"#)
        );
    }

    #[test]
    fn startup_change_is_rolled_back_when_config_write_fails() {
        let directory = TestDirectory::new();
        let blocking_file = directory.0.join("not-a-directory");
        fs::write(&blocking_file, b"file").unwrap();
        let store = ConfigStore::new(
            blocking_file.join("config.json"),
            directory.0.join("legacy.json"),
        );
        let startup = MockStartup::default();
        let config = valid_config(&directory.0);
        let executable = directory.0.join("MonitorManager.exe");

        let error = save_config_transactionally(&store, &startup, &executable, &config)
            .expect_err("configuration write should fail");

        assert!(matches!(error, SettingsSaveError::Config { .. }));
        assert_eq!(startup.read_command().unwrap(), None);
    }

    #[test]
    fn successful_transaction_saves_matching_startup_state() {
        let directory = TestDirectory::new();
        let store = ConfigStore::new(
            directory.0.join("config.json"),
            directory.0.join("legacy.json"),
        );
        let startup = MockStartup::default();
        let config = valid_config(&directory.0);
        let executable = directory.0.join("MonitorManager.exe");

        save_config_transactionally(&store, &startup, &executable, &config).unwrap();

        assert_eq!(
            startup.read_command().unwrap(),
            Some(startup_command(&executable).unwrap())
        );
        assert!(store.path().exists());
    }
}
