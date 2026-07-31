use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppPaths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub recovery: PathBuf,
    pub log: PathBuf,
    pub lock: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> io::Result<Self> {
        let local_app_data = known_local_app_data()?;
        Ok(Self::from_root(local_app_data.join("MonitorManager")))
    }

    pub fn from_root(root: PathBuf) -> Self {
        Self {
            config: root.join("config.json"),
            recovery: root.join("recovery.json"),
            log: root.join("monitor-manager.log"),
            lock: root.join("instance.lock"),
            root,
        }
    }

    pub fn ensure_root(&self) -> io::Result<()> {
        fs::create_dir_all(&self.root)
    }

    pub fn legacy_config_path(executable: &Path) -> PathBuf {
        executable
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("config.json")
    }
}

fn known_local_app_data() -> io::Result<PathBuf> {
    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, HANDLE::default())
            .map_err(windows_error_to_io)?;
        if raw.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Windows returned no LocalAppData path",
            ));
        }

        let mut length = 0usize;
        while *raw.0.add(length) != 0 {
            length += 1;
        }
        let value = OsString::from_wide(std::slice::from_raw_parts(raw.0, length));
        CoTaskMemFree(Some(raw.0.cast()));
        Ok(PathBuf::from(value))
    }
}

fn windows_error_to_io(error: windows::core::Error) -> io::Error {
    io::Error::other(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_all_files_from_one_root() {
        let root = PathBuf::from(r"C:\Users\test\AppData\Local\MonitorManager");
        let paths = AppPaths::from_root(root.clone());

        assert_eq!(paths.root, root);
        assert_eq!(paths.config.file_name().unwrap(), "config.json");
        assert_eq!(paths.recovery.file_name().unwrap(), "recovery.json");
        assert_eq!(paths.log.file_name().unwrap(), "monitor-manager.log");
        assert_eq!(paths.lock.file_name().unwrap(), "instance.lock");
    }

    #[test]
    fn legacy_config_is_adjacent_to_executable() {
        let executable = Path::new(r"D:\Portable\MonitorManager.exe");
        assert_eq!(
            AppPaths::legacy_config_path(executable),
            PathBuf::from(r"D:\Portable\config.json")
        );
    }

    #[test]
    fn resolves_the_current_users_local_app_data() {
        let paths = AppPaths::resolve().unwrap();
        assert_eq!(paths.root.file_name().unwrap(), "MonitorManager");
        assert!(paths.root.is_absolute());
    }
}
