use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::mem;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use thiserror::Error;
use windows::core::{HRESULT, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, ERROR_NO_MORE_FILES, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProcessScan {
    pub matched_triggers: Vec<PathBuf>,
    pub used_basename_fallback: bool,
}

impl ProcessScan {
    pub fn any_running(&self) -> bool {
        !self.matched_triggers.is_empty()
    }
}

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("could not take a process snapshot: {0}")]
    Snapshot(#[source] windows::core::Error),
    #[error("could not enumerate processes: {0}")]
    Enumerate(#[source] windows::core::Error),
    #[error("trigger executable path is not absolute: {0}")]
    TriggerNotAbsolute(PathBuf),
    #[error("trigger executable does not exist: {0}")]
    TriggerMissing(PathBuf),
    #[error("trigger executable is not a file: {0}")]
    TriggerNotFile(PathBuf),
    #[error("trigger executable must have an .exe extension: {0}")]
    TriggerNotExecutable(PathBuf),
    #[error("could not resolve trigger executable {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub trait ProcessBackend: Send {
    fn scan(&mut self, trigger_apps: &[PathBuf]) -> Result<ProcessScan, ProcessError>;
}

#[derive(Default)]
pub struct NativeProcessBackend;

impl NativeProcessBackend {
    pub fn new() -> Self {
        Self
    }
}

impl ProcessBackend for NativeProcessBackend {
    fn scan(&mut self, trigger_apps: &[PathBuf]) -> Result<ProcessScan, ProcessError> {
        if trigger_apps.is_empty() {
            return Ok(ProcessScan::default());
        }

        let trigger_keys: Vec<TriggerKey> = trigger_apps
            .iter()
            .map(|path| TriggerKey::new(path))
            .collect();
        let mut matched = vec![false; trigger_apps.len()];
        let mut used_basename_fallback = false;
        let snapshot = OwnedHandle(unsafe {
            CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).map_err(ProcessError::Snapshot)?
        });
        let mut entry = PROCESSENTRY32W {
            dwSize: mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        if let Err(error) = unsafe { Process32FirstW(snapshot.0, &mut entry) } {
            if is_win32_error(&error, ERROR_NO_MORE_FILES.0) {
                return Ok(ProcessScan::default());
            }
            return Err(ProcessError::Enumerate(error));
        }

        loop {
            let executable_name = nul_terminated_os_string(&entry.szExeFile);
            let identity = ProcessIdentity {
                executable_name,
                image_path: query_process_image_path(entry.th32ProcessID),
            };
            for (index, trigger) in trigger_keys.iter().enumerate() {
                if matched[index] {
                    continue;
                }
                let match_kind = trigger.matches(&identity);
                if match_kind != MatchKind::None {
                    matched[index] = true;
                    used_basename_fallback |= match_kind == MatchKind::BasenameFallback;
                }
            }

            match unsafe { Process32NextW(snapshot.0, &mut entry) } {
                Ok(()) => {}
                Err(error) if is_win32_error(&error, ERROR_NO_MORE_FILES.0) => break,
                Err(error) => return Err(ProcessError::Enumerate(error)),
            }
        }

        Ok(ProcessScan {
            matched_triggers: trigger_apps
                .iter()
                .zip(matched)
                .filter(|(_, matched)| *matched)
                .map(|(path, _)| path.clone())
                .collect(),
            used_basename_fallback,
        })
    }
}

pub fn canonicalize_trigger_path(path: &Path) -> Result<PathBuf, ProcessError> {
    if !path.is_absolute() {
        return Err(ProcessError::TriggerNotAbsolute(path.to_path_buf()));
    }
    if !has_exe_extension(path) {
        return Err(ProcessError::TriggerNotExecutable(path.to_path_buf()));
    }
    let metadata = fs::metadata(path).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            ProcessError::TriggerMissing(path.to_path_buf())
        } else {
            ProcessError::Canonicalize {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    if !metadata.is_file() {
        return Err(ProcessError::TriggerNotFile(path.to_path_buf()));
    }
    fs::canonicalize(path).map_err(|source| ProcessError::Canonicalize {
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Clone, Debug)]
struct TriggerKey {
    full_path: String,
    basename: String,
}

impl TriggerKey {
    fn new(path: &Path) -> Self {
        Self {
            full_path: normalized_path(path),
            basename: path
                .file_name()
                .unwrap_or_else(|| OsStr::new(""))
                .to_string_lossy()
                .to_lowercase(),
        }
    }

    fn matches(&self, process: &ProcessIdentity) -> MatchKind {
        match &process.image_path {
            ImagePath::Available(path) => {
                if normalized_path(path) == self.full_path {
                    MatchKind::ExactPath
                } else {
                    MatchKind::None
                }
            }
            ImagePath::AccessDenied => {
                if process.executable_name.to_string_lossy().to_lowercase() == self.basename {
                    MatchKind::BasenameFallback
                } else {
                    MatchKind::None
                }
            }
            ImagePath::Unavailable => MatchKind::None,
        }
    }
}

#[derive(Clone, Debug)]
struct ProcessIdentity {
    executable_name: OsString,
    image_path: ImagePath,
}

#[derive(Clone, Debug)]
enum ImagePath {
    Available(PathBuf),
    AccessDenied,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MatchKind {
    None,
    ExactPath,
    BasenameFallback,
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn query_process_image_path(process_id: u32) -> ImagePath {
    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
    {
        Ok(handle) => OwnedHandle(handle),
        Err(error) if is_win32_error(&error, ERROR_ACCESS_DENIED.0) => {
            return ImagePath::AccessDenied
        }
        Err(_) => return ImagePath::Unavailable,
    };

    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    match unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    } {
        Ok(()) => {
            buffer.truncate(length as usize);
            let path = PathBuf::from(OsString::from_wide(&buffer));
            ImagePath::Available(fs::canonicalize(&path).unwrap_or(path))
        }
        Err(error) if is_win32_error(&error, ERROR_ACCESS_DENIED.0) => ImagePath::AccessDenied,
        Err(_) => ImagePath::Unavailable,
    }
}

fn nul_terminated_os_string(buffer: &[u16]) -> OsString {
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    OsString::from_wide(&buffer[..length])
}

fn normalized_path(path: &Path) -> String {
    let mut value = path.as_os_str().to_string_lossy().replace('/', "\\");
    if value
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(r"\\?\UNC\"))
    {
        value = format!(r"\\{}", &value[8..]);
    } else if value
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(r"\\?\"))
    {
        value = value[4..].to_owned();
    }
    value.to_lowercase()
}

fn has_exe_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("exe"))
}

fn is_win32_error(error: &windows::core::Error, code: u32) -> bool {
    error.code() == HRESULT::from_win32(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str, image_path: ImagePath) -> ProcessIdentity {
        ProcessIdentity {
            executable_name: OsString::from(name),
            image_path,
        }
    }

    #[test]
    fn exact_paths_match_case_insensitively() {
        let trigger = TriggerKey::new(Path::new(r"C:\Games\Target.EXE"));
        let process = identity(
            "Target.exe",
            ImagePath::Available(PathBuf::from(r"c:\games\target.exe")),
        );
        assert_eq!(trigger.matches(&process), MatchKind::ExactPath);
    }

    #[test]
    fn known_wrong_path_never_uses_basename() {
        let trigger = TriggerKey::new(Path::new(r"C:\Games\Target.exe"));
        let process = identity(
            "Target.exe",
            ImagePath::Available(PathBuf::from(r"C:\Other\Target.exe")),
        );
        assert_eq!(trigger.matches(&process), MatchKind::None);
    }

    #[test]
    fn access_denied_allows_basename_fallback() {
        let trigger = TriggerKey::new(Path::new(r"C:\Games\Target.exe"));
        let process = identity("TARGET.EXE", ImagePath::AccessDenied);
        assert_eq!(trigger.matches(&process), MatchKind::BasenameFallback);
    }

    #[test]
    fn unrelated_query_failure_never_uses_basename() {
        let trigger = TriggerKey::new(Path::new(r"C:\Games\Target.exe"));
        let process = identity("Target.exe", ImagePath::Unavailable);
        assert_eq!(trigger.matches(&process), MatchKind::None);
    }

    #[test]
    fn extended_length_prefixes_normalize_to_win32_paths() {
        assert_eq!(
            normalized_path(Path::new(r"\\?\C:\Games\Target.exe")),
            normalized_path(Path::new(r"C:\Games\Target.exe"))
        );
        assert_eq!(
            normalized_path(Path::new(r"\\?\UNC\server\share\Target.exe")),
            normalized_path(Path::new(r"\\server\share\Target.exe"))
        );
    }

    #[test]
    fn native_scanner_finds_the_current_process_by_exact_path() {
        let executable = std::env::current_exe().unwrap();
        let mut backend = NativeProcessBackend::new();

        let scan = backend.scan(std::slice::from_ref(&executable)).unwrap();

        assert_eq!(scan.matched_triggers, vec![executable]);
        assert!(!scan.used_basename_fallback);
    }
}
