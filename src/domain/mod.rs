use std::fmt;

pub const MAX_FILES_PER_RUN: usize = 250_000;
pub const MAX_WORKERS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JpegQuality(u8);

impl JpegQuality {
    pub fn new(value: i32) -> Result<Self, ValidationError> {
        let value = u8::try_from(value).map_err(|_| ValidationError::InvalidQuality)?;
        if (1..=100).contains(&value) {
            Ok(Self(value))
        } else {
            Err(ValidationError::InvalidQuality)
        }
    }

    pub fn get(self) -> u8 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerCount(usize);

impl WorkerCount {
    pub fn new(value: i32) -> Result<Self, ValidationError> {
        let value = usize::try_from(value).map_err(|_| ValidationError::InvalidWorkerCount)?;
        if (1..=MAX_WORKERS).contains(&value) {
            Ok(Self(value))
        } else {
            Err(ValidationError::InvalidWorkerCount)
        }
    }

    pub fn get(self) -> usize {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessingPolicy {
    pub quality: JpegQuality,
    pub workers: WorkerCount,
    pub verify_hash: bool,
    pub process_mpo: bool,
    pub delete_thumbs: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Scanning,
    Processing,
    Cancelling,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    AlreadyOptimized { duplicate_count: usize },
    NotSmaller,
    MpoDisabled,
    UnsupportedFormat,
    ResourceLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageOutcome {
    Success { before: u64, after: u64 },
    Partial { before: u64, after: u64 },
    Skipped(SkipReason),
    Failed,
    Cancelled,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RunStats {
    pub total_found: usize,
    pub completed: usize,
    pub optimized: usize,
    pub skipped: usize,
    pub errors: usize,
    pub partial: usize,
    pub thumbs_deleted: usize,
    pub thumbs_errors: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

impl RunStats {
    pub fn register(&mut self, outcome: &ImageOutcome) {
        match outcome {
            ImageOutcome::Success { before, after } => {
                self.completed += 1;
                self.optimized += 1;
                self.bytes_before = self.bytes_before.saturating_add(*before);
                self.bytes_after = self.bytes_after.saturating_add(*after);
            }
            ImageOutcome::Partial { before, after } => {
                self.completed += 1;
                self.optimized += 1;
                self.partial += 1;
                self.bytes_before = self.bytes_before.saturating_add(*before);
                self.bytes_after = self.bytes_after.saturating_add(*after);
            }
            ImageOutcome::Skipped(_) => {
                self.completed += 1;
                self.skipped += 1;
            }
            ImageOutcome::Failed => {
                self.completed += 1;
                self.errors += 1;
            }
            ImageOutcome::Cancelled => {}
        }
    }

    pub fn saved_bytes(&self) -> i128 {
        i128::from(self.bytes_before) - i128::from(self.bytes_after)
    }

    pub fn total_errors(&self) -> usize {
        self.errors.saturating_add(self.thumbs_errors)
    }

    pub fn has_incidents(&self) -> bool {
        self.total_errors() > 0 || self.partial > 0
    }

    pub fn progress(&self) -> f32 {
        if self.total_found == 0 {
            0.0
        } else {
            (self.completed as f32 / self.total_found as f32).clamp(0.0, 1.0)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationError {
    EmptyPath,
    PathTooLong,
    InvalidQuality,
    InvalidWorkerCount,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyPath => "select an image folder",
            Self::PathTooLong => "the path exceeds the supported limit",
            Self::InvalidQuality => "quality must be between 1 and 100",
            Self::InvalidWorkerCount => "worker count must be between 1 and 32",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ValidationError {}

#[cfg(test)]
mod tests {
    use super::{ImageOutcome, JpegQuality, RunStats, SkipReason, WorkerCount};

    #[test]
    fn quality_rejects_values_outside_the_closed_range() {
        assert!(JpegQuality::new(0).is_err());
        assert!(JpegQuality::new(1).is_ok());
        assert!(JpegQuality::new(100).is_ok());
        assert!(JpegQuality::new(101).is_err());
    }

    #[test]
    fn workers_reject_values_outside_the_closed_range() {
        assert!(WorkerCount::new(0).is_err());
        assert!(WorkerCount::new(1).is_ok());
        assert!(WorkerCount::new(32).is_ok());
        assert!(WorkerCount::new(33).is_err());
    }

    #[test]
    fn stats_keep_partial_writes_visible() {
        let mut stats = RunStats {
            total_found: 2,
            ..RunStats::default()
        };
        stats.register(&ImageOutcome::Partial {
            before: 1_000,
            after: 400,
        });
        stats.register(&ImageOutcome::Skipped(SkipReason::AlreadyOptimized {
            duplicate_count: 1,
        }));

        assert_eq!(stats.completed, 2);
        assert_eq!(stats.optimized, 1);
        assert_eq!(stats.partial, 1);
        assert_eq!(stats.saved_bytes(), 600);
        assert_eq!(stats.progress(), 1.0);

        stats.thumbs_errors = 2;
        assert_eq!(stats.total_errors(), 2);
        assert!(stats.has_incidents());
    }
}
