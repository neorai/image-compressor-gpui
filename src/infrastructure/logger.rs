use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ports::{ActivityLog, LogLevel};

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

pub struct FileActivityLog {
    state: Mutex<LogFile>,
}

struct LogFile {
    path: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
}

impl FileActivityLog {
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_with_limit(path, MAX_LOG_BYTES)
    }

    fn open_with_limit(path: &Path, max_bytes: u64) -> io::Result<Self> {
        if file_len(path)? > max_bytes {
            rotate(path)?;
        }
        let file = open_append(path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            state: Mutex::new(LogFile {
                path: path.to_path_buf(),
                file,
                written,
                max_bytes,
            }),
        })
    }
}

impl ActivityLog for FileActivityLog {
    fn write(&self, level: LogLevel, operation: &str, message: &str) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());
        let level = match level {
            LogLevel::Info => "INFO",
            LogLevel::Warning => "WARN",
            LogLevel::Error => "ERROR",
        };
        let operation = sanitized(operation, 64);
        let message = sanitized(message, 1_000);
        let line = format!("{timestamp} {level} {operation} {message}\n");

        match self.state.lock() {
            Ok(mut log) => {
                if let Err(error) = log.append(line.as_bytes()) {
                    eprintln!("the activity log could not be written: {error}");
                }
            }
            Err(error) => eprintln!("the activity log lock became unusable: {error}"),
        }
    }
}

impl LogFile {
    fn append(&mut self, line: &[u8]) -> io::Result<()> {
        let length = u64::try_from(line.len()).unwrap_or(u64::MAX);
        if self.written > 0 && self.written.saturating_add(length) > self.max_bytes {
            // The open handle allows renaming, so the current file can be
            // moved aside before a fresh one replaces it.
            rotate(&self.path)?;
            self.file = open_append(&self.path)?;
            self.written = 0;
        }
        self.file.write_all(line)?;
        self.written = self.written.saturating_add(length);
        Ok(())
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn file_len(path: &Path) -> io::Result<u64> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

fn rotate(path: &Path) -> io::Result<()> {
    let backup = path.with_extension("log.1");
    match fs::remove_file(&backup) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::rename(path, backup)
}

fn sanitized(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .take(max_chars)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{FileActivityLog, sanitized};
    use crate::ports::{ActivityLog, LogLevel};

    #[test]
    fn log_fields_cannot_inject_new_lines() {
        assert_eq!(sanitized("one\ntwo\r", 20), "one two ");
    }

    #[test]
    fn log_fields_are_bounded_by_characters() {
        assert_eq!(sanitized("áéíóú", 3), "áéí");
    }

    #[test]
    fn log_rotates_while_the_application_is_running() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let path = directory.path().join("compression.log");
        let log = FileActivityLog::open_with_limit(&path, 64)?;

        for _ in 0..4 {
            log.write(
                LogLevel::Info,
                "image",
                "a message long enough to fill the log",
            );
        }
        drop(log);

        let backup = directory.path().join("compression.log.1");
        assert!(backup.is_file());
        assert!(std::fs::metadata(&path)?.len() <= 64);
        assert!(std::fs::metadata(&backup)?.len() <= 64);
        Ok(())
    }
}
