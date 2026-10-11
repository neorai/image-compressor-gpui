use std::fmt;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warning,
    Error,
}

pub trait ActivityLog: Send + Sync {
    fn write(&self, level: LogLevel, operation: &str, message: &str);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessedImage {
    pub duplicate_count: usize,
}

pub trait ImageRegistry {
    fn processed_image(
        &mut self,
        hash: &str,
        path: &Path,
    ) -> Result<Option<ProcessedImage>, RegistryError>;

    fn record_image(&mut self, hash: &str, path: &Path, quality: u8) -> Result<(), RegistryError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryError {
    diagnostic: String,
}

impl RegistryError {
    pub fn new(diagnostic: impl Into<String>) -> Self {
        Self {
            diagnostic: diagnostic.into(),
        }
    }
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.diagnostic)
    }
}

impl std::error::Error for RegistryError {}
