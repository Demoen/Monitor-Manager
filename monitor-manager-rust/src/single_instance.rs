use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::{w, Error, Result};
use windows::Win32::Foundation::{
    ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION, HANDLE, HWND, LPARAM, WPARAM,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetPropW, PostMessageW, RegisterWindowMessageW, SetPropW, HWND_BROADCAST,
};

const ACTIVATION_MESSAGE_NAME: windows::core::PCWSTR = w!("MonitorManager.Activate.Settings.v1");
const ACTIVATION_READY_PROPERTY: windows::core::PCWSTR = w!("MonitorManager.ActivationReady.v1");

#[derive(Debug)]
pub struct InstanceGuard {
    file: File,
    path: PathBuf,
}

impl InstanceGuard {
    pub fn try_acquire(path: &Path) -> io::Result<Option<Self>> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let mut file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(path)
        {
            Ok(file) => file,
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(code)
                        if code == ERROR_SHARING_VIOLATION.0 as i32
                            || code == ERROR_LOCK_VIOLATION.0 as i32
                ) =>
            {
                return Ok(None)
            }
            Err(error) => return Err(error),
        };

        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        writeln!(file, "{}", std::process::id())?;
        file.sync_all()?;
        Ok(Some(Self {
            file,
            path: path.to_path_buf(),
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file(&self) -> &File {
        &self.file
    }
}

pub fn activation_message() -> u32 {
    unsafe { RegisterWindowMessageW(ACTIVATION_MESSAGE_NAME) }
}

pub fn broadcast_activation() -> Result<()> {
    let message = activation_message();
    if message == 0 {
        return Err(Error::from_win32());
    }
    unsafe { PostMessageW(HWND_BROADCAST, message, WPARAM(0), LPARAM(0)) }
}

pub fn activate_existing(timeout: Duration) -> Result<bool> {
    let message = activation_message();
    if message == 0 {
        return Err(Error::from_win32());
    }
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(window) = unsafe { FindWindowW(None, w!("Monitor Manager Settings")) } {
            let ready = unsafe { GetPropW(window, ACTIVATION_READY_PROPERTY) };
            if !ready.is_invalid() {
                unsafe { PostMessageW(window, message, WPARAM(0), LPARAM(0))? };
                return Ok(true);
            }
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

pub fn mark_activation_ready(window: HWND) -> Result<()> {
    unsafe { SetPropW(window, ACTIVATION_READY_PROPERTY, HANDLE(window.0)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn lock_is_exclusive_and_can_be_reacquired_after_drop() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "monitor-manager-instance-{}-{suffix}",
            std::process::id()
        ));
        let path = directory.join("instance.lock");

        let first = InstanceGuard::try_acquire(&path).unwrap().unwrap();
        assert!(InstanceGuard::try_acquire(&path).unwrap().is_none());
        drop(first);
        assert!(InstanceGuard::try_acquire(&path).unwrap().is_some());

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn registered_activation_message_is_stable() {
        let first = activation_message();
        assert_ne!(first, 0);
        assert_eq!(first, activation_message());
    }
}
