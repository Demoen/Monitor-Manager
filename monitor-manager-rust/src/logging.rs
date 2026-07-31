use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct FileLogger {
    path: PathBuf,
    max_bytes: u64,
    backups: usize,
    enabled: bool,
    gate: Mutex<()>,
}

impl FileLogger {
    pub fn open(path: &Path, max_bytes: u64, backups: usize) -> io::Result<Self> {
        if max_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log size limit must be greater than zero",
            ));
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            max_bytes,
            backups,
            enabled: true,
            gate: Mutex::new(()),
        })
    }

    pub fn disabled(path: PathBuf) -> Self {
        Self {
            path,
            max_bytes: 1,
            backups: 0,
            enabled: false,
            gate: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write(&self, level: &str, message: impl AsRef<str>) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let guard = self
            .gate
            .lock()
            .map_err(|_| io::Error::other("log lock is poisoned"))?;
        self.write_locked(guard, level, message.as_ref())
    }

    pub fn try_write(&self, level: &str, message: impl AsRef<str>) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let guard = match self.gate.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "log is busy"))
            }
            Err(TryLockError::Poisoned(_)) => return Err(io::Error::other("log lock is poisoned")),
        };
        self.write_locked(guard, level, message.as_ref())
    }

    pub fn info(&self, message: impl AsRef<str>) -> io::Result<()> {
        self.write("INFO", message)
    }

    pub fn warn(&self, message: impl AsRef<str>) -> io::Result<()> {
        self.write("WARN", message)
    }

    pub fn error(&self, message: impl AsRef<str>) -> io::Result<()> {
        self.write("ERROR", message)
    }

    fn write_locked(
        &self,
        _guard: MutexGuard<'_, ()>,
        level: &str,
        message: &str,
    ) -> io::Result<()> {
        let mut entry = format_entry(level, message);
        truncate_entry(&mut entry, self.max_bytes);
        let current_size = fs::metadata(&self.path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if current_size > 0 && current_size.saturating_add(entry.len() as u64) > self.max_bytes {
            self.rotate()?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(entry.as_bytes())?;
        file.flush()?;
        file.sync_data()
    }

    fn rotate(&self) -> io::Result<()> {
        if self.backups == 0 {
            OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&self.path)?
                .sync_all()?;
            return Ok(());
        }

        let oldest = backup_path(&self.path, self.backups);
        if oldest.exists() {
            fs::remove_file(oldest)?;
        }
        for index in (1..self.backups).rev() {
            let source = backup_path(&self.path, index);
            if !source.exists() {
                continue;
            }
            let destination = backup_path(&self.path, index + 1);
            if destination.exists() {
                fs::remove_file(&destination)?;
            }
            fs::rename(source, destination)?;
        }
        if self.path.exists() {
            fs::rename(&self.path, backup_path(&self.path, 1))?;
        }
        Ok(())
    }
}

pub fn install_panic_hook(logger: Arc<FileLogger>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let payload = panic_info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| {
                panic_info
                    .payload()
                    .downcast_ref::<String>()
                    .map(String::as_str)
            })
            .unwrap_or("non-string panic payload");
        let location = panic_info
            .location()
            .map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_else(|| "unknown location".to_owned());
        let _ = logger.try_write("PANIC", format!("{payload} at {location}"));
        previous(panic_info);
    }));
}

fn format_entry(level: &str, message: &str) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let level = level.trim().to_uppercase();
    let message = message.replace('\r', "\\r").replace('\n', "\\n");
    format!("[{timestamp}] {level}: {message}\n")
}

fn truncate_entry(entry: &mut String, max_bytes: u64) {
    let Ok(max_bytes) = usize::try_from(max_bytes) else {
        return;
    };
    if entry.len() <= max_bytes {
        return;
    }
    if max_bytes == 1 {
        entry.clear();
        entry.push('\n');
        return;
    }
    let mut boundary = max_bytes - 1;
    while !entry.is_char_boundary(boundary) {
        boundary -= 1;
    }
    entry.truncate(boundary);
    entry.push('\n');
}

fn backup_path(path: &Path, index: usize) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(format!(".{index}"));
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "monitor-manager-log-{}-{suffix}",
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

    #[test]
    fn writes_single_line_entries_and_flushes_them() {
        let directory = TestDirectory::new();
        let path = directory.0.join("app.log");
        let logger = FileLogger::open(&path, 1024, 2).unwrap();

        logger.info("first\nsecond").unwrap();

        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("INFO: first\\nsecond"));
        assert_eq!(contents.lines().count(), 1);
    }

    #[test]
    fn rotates_and_caps_each_log_file() {
        let directory = TestDirectory::new();
        let path = directory.0.join("app.log");
        let logger = FileLogger::open(&path, 80, 2).unwrap();

        for index in 0..12 {
            logger.info(format!("entry {index} with padding")).unwrap();
        }

        assert!(backup_path(&path, 1).exists());
        assert!(fs::metadata(&path).unwrap().len() <= 80);
        assert!(fs::metadata(backup_path(&path, 1)).unwrap().len() <= 80);
        assert!(!backup_path(&path, 3).exists());
    }

    #[test]
    fn oversized_entries_are_truncated_at_utf8_boundaries() {
        let directory = TestDirectory::new();
        let path = directory.0.join("app.log");
        let logger = FileLogger::open(&path, 32, 1).unwrap();

        logger.error("é".repeat(100)).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.is_char_boundary(contents.len()));
        assert!(fs::metadata(path).unwrap().len() <= 32);
    }
}
