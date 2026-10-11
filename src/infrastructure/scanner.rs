use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::domain::MAX_FILES_PER_RUN;

const MAX_DIRECTORY_DEPTH: usize = 128;
const MAX_SCANNED_ENTRIES: usize = 1_000_000;

#[derive(Debug, Default)]
pub struct ScanResult {
    pub images: Vec<PathBuf>,
    pub thumbs_databases: Vec<PathBuf>,
    pub warnings: Vec<ScanWarning>,
}

#[derive(Debug)]
pub struct ScanWarning {
    pub code: &'static str,
    pub diagnostic: String,
}

pub fn scan_directory(root: &Path, cancelled: &AtomicBool) -> Result<ScanResult, ScanError> {
    let mut result = ScanResult::default();
    let mut directories = VecDeque::from([(root.to_path_buf(), 0_usize)]);
    let mut scanned_entries = 0_usize;

    while let Some((directory, depth)) = directories.pop_front() {
        if cancelled.load(Ordering::Relaxed) {
            return Err(ScanError::Cancelled);
        }
        if depth > MAX_DIRECTORY_DEPTH {
            result.warnings.push(ScanWarning {
                code: "scan-depth-limit",
                diagnostic: format!(
                    "a folder deeper than {MAX_DIRECTORY_DEPTH} levels was skipped"
                ),
            });
            continue;
        }

        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if directory != root => {
                result.warnings.push(ScanWarning {
                    code: "scan-subfolder-unreadable",
                    diagnostic: format!("a subfolder could not be read: {error}"),
                });
                continue;
            }
            Err(error) => return Err(ScanError::Io(error)),
        };

        for entry in entries {
            if cancelled.load(Ordering::Relaxed) {
                return Err(ScanError::Cancelled);
            }
            scanned_entries = scanned_entries.saturating_add(1);
            if scanned_entries > MAX_SCANNED_ENTRIES {
                return Err(ScanError::TooManyEntries);
            }

            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    result.warnings.push(ScanWarning {
                        code: "scan-entry-unreadable",
                        diagnostic: format!("a directory entry could not be read: {error}"),
                    });
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    result.warnings.push(ScanWarning {
                        code: "scan-entry-type-unavailable",
                        diagnostic: format!("an entry type could not be identified: {error}"),
                    });
                    continue;
                }
            };
            if file_type.is_symlink() {
                result.warnings.push(ScanWarning {
                    code: "scan-link-skipped",
                    diagnostic: "a symbolic link or junction was skipped".to_owned(),
                });
                continue;
            }

            let path = entry.path();
            if file_type.is_dir() {
                directories.push_back((path, depth.saturating_add(1)));
            } else if file_type.is_file() {
                if is_jpeg_path(&path) {
                    if result.images.len() >= MAX_FILES_PER_RUN {
                        return Err(ScanError::TooManyImages);
                    }
                    result.images.push(path);
                } else if is_thumbs_database(&path) {
                    result.thumbs_databases.push(path);
                }
            }
        }
    }

    result.images.sort_unstable();
    result.thumbs_databases.sort_unstable();
    Ok(result)
}

pub fn cleanup_thumbs_databases(
    canonical_root: &Path,
    candidates: &[PathBuf],
    cancelled: &AtomicBool,
) -> CleanupStats {
    let mut stats = CleanupStats::default();
    for candidate in candidates {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        match fs::symlink_metadata(candidate) {
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_file() => {}
            Ok(_) | Err(_) => {
                stats.errors += 1;
                continue;
            }
        }
        if !is_thumbs_database(candidate) {
            stats.errors += 1;
            continue;
        }
        let canonical = match candidate.canonicalize() {
            Ok(path) => path,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        if !canonical.starts_with(canonical_root) {
            stats.errors += 1;
            continue;
        }
        match fs::remove_file(candidate) {
            Ok(()) => stats.deleted += 1,
            Err(_) => stats.errors += 1,
        }
    }
    stats
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CleanupStats {
    pub deleted: usize,
    pub errors: usize,
}

fn is_jpeg_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg")
        })
}

fn is_thumbs_database(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("thumbs.db"))
}

#[derive(Debug)]
pub enum ScanError {
    Io(io::Error),
    TooManyEntries,
    TooManyImages,
    Cancelled,
}

impl fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "the folder could not be scanned: {error}"),
            Self::TooManyEntries => formatter.write_str("the folder exceeds 1,000,000 entries"),
            Self::TooManyImages => write!(
                formatter,
                "the folder exceeds the limit of {MAX_FILES_PER_RUN} images"
            ),
            Self::Cancelled => formatter.write_str("scan cancelled"),
        }
    }
}

impl std::error::Error for ScanError {}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::AtomicBool;

    use tempfile::tempdir;

    use super::{cleanup_thumbs_databases, scan_directory};

    #[test]
    fn scan_is_recursive_and_extension_is_case_insensitive()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let nested = directory.path().join("nested");
        fs::create_dir(&nested)?;
        fs::write(nested.join("one.JPG"), b"jpeg")?;
        fs::write(nested.join("two.png"), b"png")?;
        fs::write(nested.join("Thumbs.db"), b"cache")?;

        let result = scan_directory(directory.path(), &AtomicBool::new(false))?;

        assert_eq!(result.images.len(), 1);
        assert_eq!(result.thumbs_databases.len(), 1);
        Ok(())
    }

    #[test]
    fn cleanup_only_deletes_collected_thumbs_files() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let root = directory.path().canonicalize()?;
        let thumbs = root.join("Thumbs.db");
        let keep = root.join("keep.db");
        fs::write(&thumbs, b"cache")?;
        fs::write(&keep, b"data")?;

        let stats = cleanup_thumbs_databases(
            &root,
            &[thumbs.clone(), keep.clone()],
            &AtomicBool::new(false),
        );

        assert_eq!(stats.deleted, 1);
        assert_eq!(stats.errors, 1);
        assert!(!thumbs.exists());
        assert!(keep.exists());
        Ok(())
    }
}
