use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub database: PathBuf,
    pub log: PathBuf,
}

impl AppPaths {
    pub fn discover() -> io::Result<Self> {
        Self::from_executable(&std::env::current_exe()?)
    }

    fn from_executable(executable: &Path) -> io::Result<Self> {
        let executable_dir = executable.parent().ok_or_else(|| {
            io::Error::other("the executable path does not have a parent directory")
        })?;
        let data_dir = executable_dir.join("data");
        std::fs::create_dir_all(&data_dir)?;

        Ok(Self {
            database: data_dir.join("images.db"),
            log: data_dir.join("compression.log"),
            data_dir,
        })
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::AppPaths;

    #[test]
    fn data_directory_is_created_next_to_the_executable() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempdir()?;
        let executable = directory.path().join("image-compressor.exe");

        let paths = AppPaths::from_executable(&executable)?;

        assert_eq!(paths.data_dir, directory.path().join("data"));
        assert!(paths.data_dir.is_dir());
        assert_eq!(paths.database, paths.data_dir.join("images.db"));
        Ok(())
    }
}
