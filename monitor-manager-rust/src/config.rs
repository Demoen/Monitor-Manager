use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MOVE_FILE_FLAGS,
};

pub const CONFIG_VERSION: u32 = 2;

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static CORRUPT_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DisabledDisplay {
    pub device_path: String,
    pub last_known_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConfigV2 {
    #[serde(alias = "version")]
    pub schema_version: u32,
    pub trigger_apps: Vec<PathBuf>,
    pub disabled_displays: Vec<DisabledDisplay>,
    pub automation_enabled: bool,
    pub start_with_windows: bool,
}

impl Default for ConfigV2 {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_VERSION,
            trigger_apps: Vec::new(),
            disabled_displays: Vec::new(),
            automation_enabled: false,
            start_with_windows: false,
        }
    }
}

impl ConfigV2 {
    pub fn is_configured(&self) -> bool {
        !self.trigger_apps.is_empty() && !self.disabled_displays.is_empty()
    }

    pub fn validate(&self) -> Result<(), ConfigValidationError> {
        self.normalized_for_save().map(|_| ())
    }

    pub fn validate_persisted(&self) -> Result<(), ConfigValidationError> {
        if self.schema_version != CONFIG_VERSION {
            return Err(ConfigValidationError::UnsupportedSchema {
                found: self.schema_version,
            });
        }
        if self.trigger_apps.is_empty() {
            return Err(ConfigValidationError::NoTriggerApps);
        }
        if self.disabled_displays.is_empty() {
            return Err(ConfigValidationError::NoDisabledDisplays);
        }

        let mut trigger_keys = HashSet::new();
        for path in &self.trigger_apps {
            if !path.is_absolute() {
                return Err(ConfigValidationError::TriggerNotAbsolute(path.clone()));
            }
            if !has_exe_extension(path) {
                return Err(ConfigValidationError::TriggerNotExecutable(path.clone()));
            }
            if !trigger_keys.insert(normalized_path_key(path)) {
                return Err(ConfigValidationError::DuplicateTrigger(path.clone()));
            }
        }

        validate_displays(&self.disabled_displays)
    }

    pub fn normalized_for_save(&self) -> Result<Self, ConfigValidationError> {
        if self.schema_version != CONFIG_VERSION {
            return Err(ConfigValidationError::UnsupportedSchema {
                found: self.schema_version,
            });
        }
        if self.trigger_apps.is_empty() {
            return Err(ConfigValidationError::NoTriggerApps);
        }
        if self.disabled_displays.is_empty() {
            return Err(ConfigValidationError::NoDisabledDisplays);
        }

        let mut normalized = self.clone();
        normalized.trigger_apps.clear();
        let mut trigger_keys = HashSet::new();
        for path in &self.trigger_apps {
            let canonical = canonical_executable(path)?;
            let key = normalized_path_key(&canonical);
            if !trigger_keys.insert(key) {
                return Err(ConfigValidationError::DuplicateTrigger(path.clone()));
            }
            normalized.trigger_apps.push(canonical);
        }

        validate_displays(&self.disabled_displays)?;

        Ok(normalized)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigSource {
    Current,
    FirstRun,
    MigratedLegacy,
    RecoveredCorrupt(PathBuf),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigWarning {
    TriggerMissing(PathBuf),
    TriggerNotFile(PathBuf),
    TriggerNotExecutable(PathBuf),
    TriggerNotAbsolute(PathBuf),
    CorruptConfig { backup: PathBuf, details: String },
    LegacyConfigUnreadable { path: PathBuf, details: String },
}

impl std::fmt::Display for ConfigWarning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TriggerMissing(path) => {
                write!(
                    formatter,
                    "configured executable is missing: {}",
                    path.display()
                )
            }
            Self::TriggerNotFile(path) => {
                write!(
                    formatter,
                    "configured executable is not a file: {}",
                    path.display()
                )
            }
            Self::TriggerNotExecutable(path) => {
                write!(
                    formatter,
                    "configured path is not an .exe file: {}",
                    path.display()
                )
            }
            Self::TriggerNotAbsolute(path) => {
                write!(
                    formatter,
                    "configured executable path is not absolute: {}",
                    path.display()
                )
            }
            Self::CorruptConfig { backup, details } => write!(
                formatter,
                "invalid configuration was preserved at {}: {details}",
                backup.display()
            ),
            Self::LegacyConfigUnreadable { path, details } => write!(
                formatter,
                "legacy configuration at {} could not be read: {details}",
                path.display()
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigLoad {
    pub config: ConfigV2,
    pub source: ConfigSource,
    pub warnings: Vec<ConfigWarning>,
}

#[derive(Debug, Error)]
pub enum ConfigValidationError {
    #[error("configuration schema {found} is not supported")]
    UnsupportedSchema { found: u32 },
    #[error("at least one trigger executable is required")]
    NoTriggerApps,
    #[error("at least one display must be selected")]
    NoDisabledDisplays,
    #[error("trigger executable path is not absolute: {0}")]
    TriggerNotAbsolute(PathBuf),
    #[error("trigger executable does not exist: {0}")]
    TriggerMissing(PathBuf),
    #[error("trigger executable is not a file: {0}")]
    TriggerNotFile(PathBuf),
    #[error("trigger executable must have an .exe extension: {0}")]
    TriggerNotExecutable(PathBuf),
    #[error("trigger executable is configured more than once: {0}")]
    DuplicateTrigger(PathBuf),
    #[error("a selected display has an empty device path")]
    EmptyDisplayPath,
    #[error("display is selected more than once: {0}")]
    DuplicateDisplay(String),
    #[error("could not resolve trigger executable {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("configuration schema {found} is not supported")]
    UnsupportedSchema { found: u32 },
    #[error(transparent)]
    Validation(#[from] ConfigValidationError),
    #[error("could not serialize configuration: {0}")]
    Serialize(#[source] serde_json::Error),
}

#[derive(Debug, Error)]
pub enum AtomicJsonError {
    #[error("could not serialize JSON: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("could not write JSON: {0}")]
    Io(#[from] io::Error),
}

#[derive(Clone, Debug)]
pub struct ConfigStore {
    path: PathBuf,
    legacy_path: PathBuf,
}

impl ConfigStore {
    pub fn new(path: PathBuf, legacy_path: PathBuf) -> Self {
        Self { path, legacy_path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(
        &self,
        current_secondary_displays: &[DisabledDisplay],
    ) -> Result<ConfigLoad, ConfigError> {
        match fs::read(&self.path) {
            Ok(contents) => self.load_current(&contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.load_legacy(current_secondary_displays)
            }
            Err(source) => Err(ConfigError::Io {
                action: "read",
                path: self.path.clone(),
                source,
            }),
        }
    }

    pub fn save(&self, config: &ConfigV2) -> Result<(), ConfigError> {
        let normalized = config.normalized_for_save()?;
        atomic_write_json(&self.path, &normalized).map_err(|error| match error {
            AtomicJsonError::Serialize(source) => ConfigError::Serialize(source),
            AtomicJsonError::Io(source) => ConfigError::Io {
                action: "write",
                path: self.path.clone(),
                source,
            },
        })
    }

    pub fn save_persisted(&self, config: &ConfigV2) -> Result<(), ConfigError> {
        config.validate_persisted()?;
        atomic_write_json(&self.path, config).map_err(|error| match error {
            AtomicJsonError::Serialize(source) => ConfigError::Serialize(source),
            AtomicJsonError::Io(source) => ConfigError::Io {
                action: "write",
                path: self.path.clone(),
                source,
            },
        })
    }

    fn load_current(&self, contents: &[u8]) -> Result<ConfigLoad, ConfigError> {
        let config = match serde_json::from_slice::<ConfigV2>(contents) {
            Ok(config) => config,
            Err(error) => {
                let details = error.to_string();
                let backup = self.preserve_corrupt()?;
                return Ok(ConfigLoad {
                    config: ConfigV2::default(),
                    source: ConfigSource::RecoveredCorrupt(backup.clone()),
                    warnings: vec![ConfigWarning::CorruptConfig { backup, details }],
                });
            }
        };

        if config.schema_version != CONFIG_VERSION {
            return Err(ConfigError::UnsupportedSchema {
                found: config.schema_version,
            });
        }

        Ok(ConfigLoad {
            warnings: warnings_for_config(&config),
            config,
            source: ConfigSource::Current,
        })
    }

    fn load_legacy(
        &self,
        current_secondary_displays: &[DisabledDisplay],
    ) -> Result<ConfigLoad, ConfigError> {
        if paths_equal(&self.path, &self.legacy_path) {
            return Ok(first_run());
        }

        let contents = match fs::read(&self.legacy_path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(first_run()),
            Err(source) => {
                return Err(ConfigError::Io {
                    action: "read",
                    path: self.legacy_path.clone(),
                    source,
                })
            }
        };

        let legacy = match serde_json::from_slice::<LegacyConfig>(&contents) {
            Ok(legacy) => legacy,
            Err(error) => {
                let mut load = first_run();
                load.warnings.push(ConfigWarning::LegacyConfigUnreadable {
                    path: self.legacy_path.clone(),
                    details: error.to_string(),
                });
                return Ok(load);
            }
        };

        let trigger = PathBuf::from(legacy.target_exe.trim());
        let trigger_apps = if trigger.as_os_str().is_empty() {
            Vec::new()
        } else {
            vec![canonical_executable(&trigger).unwrap_or(trigger)]
        };
        let config = ConfigV2 {
            trigger_apps,
            disabled_displays: deduplicate_displays(current_secondary_displays),
            automation_enabled: false,
            start_with_windows: false,
            ..ConfigV2::default()
        };

        Ok(ConfigLoad {
            warnings: warnings_for_config(&config),
            config,
            source: ConfigSource::MigratedLegacy,
        })
    }

    fn preserve_corrupt(&self) -> Result<PathBuf, ConfigError> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let stem = self
            .path
            .file_stem()
            .unwrap_or_else(|| self.path.as_os_str());

        for _ in 0..1000 {
            let sequence = CORRUPT_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let mut name = OsString::from(stem);
            name.push(format!(".corrupt-{timestamp}-{sequence}.json"));
            let backup = parent.join(name);
            if backup.exists() {
                continue;
            }
            fs::rename(&self.path, &backup).map_err(|source| ConfigError::Io {
                action: "preserve invalid configuration from",
                path: self.path.clone(),
                source,
            })?;
            return Ok(backup);
        }

        Err(ConfigError::Io {
            action: "choose a backup name for",
            path: self.path.clone(),
            source: io::Error::new(io::ErrorKind::AlreadyExists, "backup name collision"),
        })
    }
}

#[derive(Deserialize)]
struct LegacyConfig {
    target_exe: String,
}

pub fn atomic_write_json<T: Serialize + ?Sized>(
    path: &Path,
    value: &T,
) -> Result<(), AtomicJsonError> {
    let mut contents = serde_json::to_vec_pretty(value)?;
    contents.push(b'\n');
    atomic_write(path, &contents)?;
    Ok(())
}

pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let (temporary_path, mut temporary_file) = create_temporary_file(path)?;
    let result = (|| {
        temporary_file.write_all(contents)?;
        temporary_file.flush()?;
        temporary_file.sync_all()?;
        drop(temporary_file);
        replace_file(&temporary_path, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn create_temporary_file(path: &Path) -> io::Result<(PathBuf, File)> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;

    for _ in 0..1000 {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut name = OsString::from(".");
        name.push(file_name);
        name.push(format!(".tmp-{}-{sequence}", std::process::id()));
        let temporary_path = parent.join(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((temporary_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "temporary file name collision",
    ))
}

fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    let source = wide_null(source);
    let destination = wide_null(destination);
    let flags = MOVE_FILE_FLAGS(MOVEFILE_REPLACE_EXISTING.0 | MOVEFILE_WRITE_THROUGH.0);
    unsafe {
        MoveFileExW(PCWSTR(source.as_ptr()), PCWSTR(destination.as_ptr()), flags)
            .map_err(io::Error::other)
    }
}

fn canonical_executable(path: &Path) -> Result<PathBuf, ConfigValidationError> {
    if !path.is_absolute() {
        return Err(ConfigValidationError::TriggerNotAbsolute(
            path.to_path_buf(),
        ));
    }
    if !has_exe_extension(path) {
        return Err(ConfigValidationError::TriggerNotExecutable(
            path.to_path_buf(),
        ));
    }
    let metadata = fs::metadata(path).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            ConfigValidationError::TriggerMissing(path.to_path_buf())
        } else {
            ConfigValidationError::Canonicalize {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    if !metadata.is_file() {
        return Err(ConfigValidationError::TriggerNotFile(path.to_path_buf()));
    }
    fs::canonicalize(path).map_err(|source| ConfigValidationError::Canonicalize {
        path: path.to_path_buf(),
        source,
    })
}

fn validate_displays(displays: &[DisabledDisplay]) -> Result<(), ConfigValidationError> {
    let mut display_keys = HashSet::new();
    for display in displays {
        if display.device_path.trim().is_empty() {
            return Err(ConfigValidationError::EmptyDisplayPath);
        }
        if !display_keys.insert(display.device_path.to_lowercase()) {
            return Err(ConfigValidationError::DuplicateDisplay(
                display.device_path.clone(),
            ));
        }
    }
    Ok(())
}

fn warnings_for_config(config: &ConfigV2) -> Vec<ConfigWarning> {
    let mut warnings = Vec::new();
    for path in &config.trigger_apps {
        if !path.is_absolute() {
            warnings.push(ConfigWarning::TriggerNotAbsolute(path.clone()));
        }
        if !has_exe_extension(path) {
            warnings.push(ConfigWarning::TriggerNotExecutable(path.clone()));
        }
        match fs::metadata(path) {
            Ok(metadata) if !metadata.is_file() => {
                warnings.push(ConfigWarning::TriggerNotFile(path.clone()))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                warnings.push(ConfigWarning::TriggerMissing(path.clone()))
            }
            _ => {}
        }
    }
    warnings
}

fn has_exe_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("exe"))
}

fn normalized_path_key(path: &Path) -> String {
    path.as_os_str()
        .to_string_lossy()
        .replace('/', "\\")
        .to_lowercase()
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    normalized_path_key(left) == normalized_path_key(right)
}

fn deduplicate_displays(displays: &[DisabledDisplay]) -> Vec<DisabledDisplay> {
    let mut seen = HashSet::new();
    displays
        .iter()
        .filter(|display| seen.insert(display.device_path.to_lowercase()))
        .cloned()
        .collect()
}

fn first_run() -> ConfigLoad {
    ConfigLoad {
        config: ConfigV2::default(),
        source: ConfigSource::FirstRun,
        warnings: Vec::new(),
    }
}

fn wide_null(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "monitor-manager-{label}-{}-{sequence}",
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

    fn display() -> DisabledDisplay {
        DisabledDisplay {
            device_path: r"\\?\DISPLAY#TEST#1".to_owned(),
            last_known_name: "Test display".to_owned(),
        }
    }

    fn valid_config(directory: &Path) -> ConfigV2 {
        let executable = directory.join("game.exe");
        fs::write(&executable, b"test executable").unwrap();
        ConfigV2 {
            trigger_apps: vec![executable],
            disabled_displays: vec![display()],
            automation_enabled: true,
            ..ConfigV2::default()
        }
    }

    #[test]
    fn default_is_unconfigured_and_paused() {
        let config = ConfigV2::default();
        assert_eq!(config.schema_version, CONFIG_VERSION);
        assert!(!config.is_configured());
        assert!(!config.automation_enabled);
        assert!(!config.start_with_windows);
    }

    #[test]
    fn save_canonicalizes_and_atomically_replaces_config() {
        let directory = TestDirectory::new("save");
        let path = directory.0.join("config.json");
        let store = ConfigStore::new(path.clone(), directory.0.join("legacy.json"));
        let mut config = valid_config(&directory.0);

        store.save(&config).unwrap();
        config.automation_enabled = false;
        store.save(&config).unwrap();

        let saved: ConfigV2 = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert!(!saved.automation_enabled);
        assert!(saved.trigger_apps[0].is_absolute());
    }

    #[test]
    fn malformed_config_is_preserved() {
        let directory = TestDirectory::new("corrupt");
        let path = directory.0.join("config.json");
        fs::write(&path, b"{not json").unwrap();
        let store = ConfigStore::new(path.clone(), directory.0.join("legacy.json"));

        let load = store.load(&[]).unwrap();

        let ConfigSource::RecoveredCorrupt(backup) = load.source else {
            panic!("expected recovered corrupt source");
        };
        assert_eq!(fs::read(backup).unwrap(), b"{not json");
        assert!(!path.exists());
        assert_eq!(load.warnings.len(), 1);
    }

    #[test]
    fn missing_trigger_is_loaded_with_a_warning() {
        let directory = TestDirectory::new("missing");
        let path = directory.0.join("config.json");
        let config = ConfigV2 {
            trigger_apps: vec![directory.0.join("gone.exe")],
            disabled_displays: vec![display()],
            ..ConfigV2::default()
        };
        atomic_write_json(&path, &config).unwrap();
        let store = ConfigStore::new(path, directory.0.join("legacy.json"));

        let load = store.load(&[]).unwrap();

        assert_eq!(load.source, ConfigSource::Current);
        assert!(matches!(
            load.warnings.as_slice(),
            [ConfigWarning::TriggerMissing(_)]
        ));
    }

    #[test]
    fn paused_state_can_be_persisted_after_a_trigger_is_removed() {
        let directory = TestDirectory::new("missing-state-save");
        let path = directory.0.join("config.json");
        let store = ConfigStore::new(path.clone(), directory.0.join("legacy.json"));
        let mut config = valid_config(&directory.0);
        let trigger = config.trigger_apps[0].clone();
        fs::remove_file(&trigger).unwrap();
        config.automation_enabled = false;

        assert!(matches!(
            config.validate(),
            Err(ConfigValidationError::TriggerMissing(_))
        ));
        store.save_persisted(&config).unwrap();

        let saved: ConfigV2 = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert!(!saved.automation_enabled);
        assert_eq!(saved.trigger_apps, vec![trigger]);
    }

    #[test]
    fn legacy_config_becomes_an_unsaved_paused_draft() {
        let directory = TestDirectory::new("legacy");
        let config_path = directory.0.join("local").join("config.json");
        let legacy_path = directory.0.join("portable").join("config.json");
        fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        let executable = directory.0.join("legacy-game.exe");
        fs::write(&executable, b"exe").unwrap();
        fs::write(
            &legacy_path,
            serde_json::to_vec(&serde_json::json!({ "target_exe": executable })).unwrap(),
        )
        .unwrap();
        let store = ConfigStore::new(config_path.clone(), legacy_path);

        let load = store.load(&[display(), display()]).unwrap();

        assert_eq!(load.source, ConfigSource::MigratedLegacy);
        assert!(!load.config.automation_enabled);
        assert!(!load.config.start_with_windows);
        assert_eq!(load.config.trigger_apps.len(), 1);
        assert_eq!(load.config.disabled_displays.len(), 1);
        assert!(!config_path.exists());
    }

    #[test]
    fn save_rejects_empty_rules_and_non_executables() {
        assert!(matches!(
            ConfigV2::default().validate(),
            Err(ConfigValidationError::NoTriggerApps)
        ));

        let directory = TestDirectory::new("validation");
        let text = directory.0.join("game.txt");
        fs::write(&text, b"text").unwrap();
        let config = ConfigV2 {
            trigger_apps: vec![text],
            disabled_displays: vec![display()],
            ..ConfigV2::default()
        };
        assert!(matches!(
            config.validate(),
            Err(ConfigValidationError::TriggerNotExecutable(_))
        ));
    }

    #[test]
    fn failed_atomic_replace_preserves_the_previous_file() {
        let directory = TestDirectory::new("atomic-failure");
        let path = directory.0.join("config.json");
        fs::write(&path, b"previous").unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();

        assert!(atomic_write(&path, b"replacement").is_err());
        drop(lock);

        assert_eq!(fs::read(&path).unwrap(), b"previous");
        let leftovers = fs::read_dir(&directory.0)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(leftovers, 0);
    }
}
