use std::fmt;
use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};

use crate::ports::{ImageRegistry, ProcessedImage, RegistryError};

const SCHEMA_VERSION: i64 = 1;
const REPORT_GROUP_LIMIT: usize = 100;

pub struct ImageDatabase {
    connection: Connection,
}

impl ImageDatabase {
    pub fn initialize(path: &Path) -> Result<(), DatabaseError> {
        let mut database = Self::open(path)?;
        database.migrate()
    }

    pub fn open(path: &Path) -> Result<Self, DatabaseError> {
        let connection = Connection::open(path).map_err(DatabaseError::Sqlite)?;
        connection
            .busy_timeout(Duration::from_secs(30))
            .map_err(DatabaseError::Sqlite)?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
            .map_err(DatabaseError::Sqlite)?;
        Ok(Self { connection })
    }

    fn migrate(&mut self) -> Result<(), DatabaseError> {
        let version: i64 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(DatabaseError::Sqlite)?;
        if version > SCHEMA_VERSION {
            return Err(DatabaseError::UnsupportedSchema(version));
        }

        if version == 0 {
            let transaction = self
                .connection
                .transaction()
                .map_err(DatabaseError::Sqlite)?;
            transaction
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS optimized_images (
                        hash TEXT NOT NULL,
                        path TEXT NOT NULL PRIMARY KEY,
                        quality INTEGER NOT NULL CHECK (quality BETWEEN 1 AND 100),
                        processed_date TEXT NOT NULL
                    );
                    CREATE INDEX IF NOT EXISTS idx_hash ON optimized_images(hash);
                    PRAGMA user_version = 1;",
                )
                .map_err(DatabaseError::Sqlite)?;
            transaction.commit().map_err(DatabaseError::Sqlite)?;
        }
        Ok(())
    }
}

impl ImageRegistry for ImageDatabase {
    fn processed_image(
        &mut self,
        hash: &str,
        path: &Path,
    ) -> Result<Option<ProcessedImage>, RegistryError> {
        let path = path_to_text(path)?;
        let count: i64 = self
            .connection
            .query_row(
                "SELECT CASE
                    WHEN EXISTS (
                        SELECT 1 FROM optimized_images WHERE hash = ?1 AND path = ?2
                    )
                    THEN (SELECT COUNT(*) FROM optimized_images WHERE hash = ?1)
                    ELSE 0
                 END",
                params![hash, path],
                |row| row.get(0),
            )
            .map_err(registry_error)?;
        Ok((count > 0).then(|| ProcessedImage {
            duplicate_count: non_negative_usize(count),
        }))
    }

    fn record_image(&mut self, hash: &str, path: &Path, quality: u8) -> Result<(), RegistryError> {
        let path = path_to_text(path)?;
        self.connection
            .execute(
                "INSERT INTO optimized_images (hash, path, quality, processed_date)
                 VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                 ON CONFLICT(path) DO UPDATE SET
                    hash = excluded.hash,
                    quality = excluded.quality,
                    processed_date = excluded.processed_date",
                params![hash, path, quality],
            )
            .map(|_| ())
            .map_err(registry_error)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DuplicateReport {
    pub total_records: usize,
    pub unique_hashes: usize,
    pub duplicate_files: usize,
    pub total_groups: usize,
    pub visible_wasted_bytes: u64,
    pub groups: Vec<DuplicateGroup>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    pub hash_prefix: String,
    pub copies: usize,
    pub wasted_bytes: u64,
    pub locations: String,
}

pub fn load_duplicate_report(path: &Path) -> Result<DuplicateReport, DatabaseError> {
    if !path.exists() {
        return Ok(DuplicateReport::default());
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(DatabaseError::Sqlite)?;
    connection
        .busy_timeout(Duration::from_secs(30))
        .map_err(DatabaseError::Sqlite)?;

    let total_records: i64 = connection
        .query_row("SELECT COUNT(*) FROM optimized_images", [], |row| {
            row.get(0)
        })
        .map_err(DatabaseError::Sqlite)?;
    let unique_hashes: i64 = connection
        .query_row(
            "SELECT COUNT(DISTINCT hash) FROM optimized_images",
            [],
            |row| row.get(0),
        )
        .map_err(DatabaseError::Sqlite)?;
    let total_groups: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM (
                SELECT hash FROM optimized_images GROUP BY hash HAVING COUNT(*) > 1
             )",
            [],
            |row| row.get(0),
        )
        .map_err(DatabaseError::Sqlite)?;

    let mut statement = connection
        .prepare(
            "SELECT hash, COUNT(*) AS copies
             FROM optimized_images
             GROUP BY hash
             HAVING COUNT(*) > 1
             ORDER BY copies DESC, hash
             LIMIT ?1",
        )
        .map_err(DatabaseError::Sqlite)?;
    let rows = statement
        .query_map(params![REPORT_GROUP_LIMIT as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(DatabaseError::Sqlite)?;
    let mut group_keys = Vec::new();
    for row in rows {
        group_keys.push(row.map_err(DatabaseError::Sqlite)?);
    }
    drop(statement);

    let mut groups = Vec::with_capacity(group_keys.len());
    let mut visible_wasted_bytes = 0_u64;
    for (hash, copies) in group_keys {
        let mut path_statement = connection
            .prepare("SELECT path FROM optimized_images WHERE hash = ?1 ORDER BY path")
            .map_err(DatabaseError::Sqlite)?;
        let path_rows = path_statement
            .query_map(params![hash], |row| row.get::<_, String>(0))
            .map_err(DatabaseError::Sqlite)?;
        let mut locations = Vec::new();
        let mut sizes = Vec::new();
        for row in path_rows {
            let location = row.map_err(DatabaseError::Sqlite)?;
            if let Ok(metadata) = fs::metadata(&location) {
                sizes.push(metadata.len());
            }
            locations.push(location);
        }
        let wasted_bytes = wasted_space(&sizes);
        visible_wasted_bytes = visible_wasted_bytes.saturating_add(wasted_bytes);
        groups.push(DuplicateGroup {
            hash_prefix: hash.chars().take(16).collect(),
            copies: non_negative_usize(copies),
            wasted_bytes,
            locations: locations.join("\n"),
        });
    }

    let total_records = non_negative_usize(total_records);
    let unique_hashes = non_negative_usize(unique_hashes);
    Ok(DuplicateReport {
        total_records,
        unique_hashes,
        duplicate_files: total_records.saturating_sub(unique_hashes),
        total_groups: non_negative_usize(total_groups),
        visible_wasted_bytes,
        groups,
    })
}

fn wasted_space(sizes: &[u64]) -> u64 {
    let Some(smallest) = sizes.iter().min() else {
        return 0;
    };
    let wasted = sizes
        .iter()
        .copied()
        .map(u128::from)
        .sum::<u128>()
        .saturating_sub(u128::from(*smallest));
    u64::try_from(wasted).unwrap_or(u64::MAX)
}

fn path_to_text(path: &Path) -> Result<&str, RegistryError> {
    path.to_str()
        .ok_or_else(|| RegistryError::new("the path does not have a valid Unicode representation"))
}

fn registry_error(error: rusqlite::Error) -> RegistryError {
    RegistryError::new(error.to_string())
}

fn non_negative_usize(value: i64) -> usize {
    usize::try_from(value.max(0)).unwrap_or(usize::MAX)
}

#[derive(Debug)]
pub enum DatabaseError {
    Sqlite(rusqlite::Error),
    UnsupportedSchema(i64),
}

impl fmt::Display for DatabaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite error: {error}"),
            Self::UnsupportedSchema(version) => {
                write!(
                    formatter,
                    "the database uses an unsupported future version: {version}"
                )
            }
        }
    }
}

impl std::error::Error for DatabaseError {}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{ImageDatabase, load_duplicate_report, wasted_space};
    use crate::ports::ImageRegistry;

    #[test]
    fn recording_same_path_replaces_its_previous_hash() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let database_path = directory.path().join("images.db");
        let image_path = directory.path().join("one.jpg");
        fs::write(&image_path, b"example")?;
        ImageDatabase::initialize(&database_path)?;
        let mut database = ImageDatabase::open(&database_path)?;

        database.record_image("old", &image_path, 50)?;
        database.record_image("new", &image_path, 75)?;

        assert!(database.processed_image("old", &image_path)?.is_none());
        assert!(database.processed_image("new", &image_path)?.is_some());
        Ok(())
    }

    #[test]
    fn report_groups_equal_hashes_at_different_paths() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let database_path = directory.path().join("images.db");
        let first = directory.path().join("one.jpg");
        let second = directory.path().join("two.jpg");
        fs::write(&first, b"same")?;
        fs::write(&second, b"same")?;
        ImageDatabase::initialize(&database_path)?;
        let mut database = ImageDatabase::open(&database_path)?;
        database.record_image("shared", &first, 50)?;
        database.record_image("shared", &second, 50)?;
        assert_eq!(
            database
                .processed_image("shared", &first)?
                .map(|image| image.duplicate_count),
            Some(2)
        );

        let report = load_duplicate_report(&database_path)?;

        assert_eq!(report.total_records, 2);
        assert_eq!(report.duplicate_files, 1);
        assert_eq!(report.total_groups, 1);
        assert_eq!(report.groups[0].copies, 2);
        assert_eq!(report.groups[0].wasted_bytes, 4);
        Ok(())
    }

    #[test]
    fn wasted_space_saturates_for_untrusted_file_sizes() {
        assert_eq!(wasted_space(&[u64::MAX, u64::MAX]), u64::MAX);
        assert_eq!(wasted_space(&[1, u64::MAX, u64::MAX]), u64::MAX);
    }
}
