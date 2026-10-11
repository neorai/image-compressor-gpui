use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};

use crate::domain::{
    ImageOutcome, JpegQuality, ProcessingPolicy, RunState, RunStats, ValidationError, WorkerCount,
};
use crate::infrastructure::database::{
    DatabaseError, DuplicateReport, ImageDatabase, load_duplicate_report,
};
use crate::infrastructure::image_processor::{ProcessedFile, process_image};
use crate::infrastructure::logger::FileActivityLog;
use crate::infrastructure::paths::AppPaths;
use crate::infrastructure::scanner::{ScanError, cleanup_thumbs_databases, scan_directory};
use crate::ports::{ActivityLog, LogLevel};

const CHANNEL_ITEMS_PER_WORKER: usize = 2;
const UI_BATCH_SIZE: usize = 16;
const UI_BATCH_INTERVAL: Duration = Duration::from_millis(100);

pub trait EventSink: Send + Sync + 'static {
    fn send(&self, event: AppEvent);
}

#[derive(Debug, Clone)]
pub enum AppEvent {
    State {
        state: RunState,
        status_code: &'static str,
        status_value: i32,
    },
    Progress {
        stats: RunStats,
        current_file: String,
    },
    Activity(Vec<ActivityEntry>),
    Finished {
        state: RunState,
        stats: RunStats,
        elapsed: Duration,
    },
}

#[derive(Debug, Clone)]
pub struct ActivityEntry {
    pub level: LogLevel,
    pub source: ActivitySource,
    pub message_code: &'static str,
    pub message_args: [i32; 3],
}

/// What an activity row refers to. Only image paths are user data; the other
/// sources are labels that the interface localizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivitySource {
    Image(String),
    Scan,
    Process,
    Database,
}

#[derive(Debug, Clone)]
pub struct RunRequest {
    root: PathBuf,
    policy: ProcessingPolicy,
}

impl RunRequest {
    pub fn new(
        path: &str,
        quality: i32,
        workers: i32,
        verify_hash: bool,
        process_mpo: bool,
        delete_thumbs: bool,
    ) -> Result<Self, ValidationError> {
        let path = path.trim();
        if path.is_empty() {
            return Err(ValidationError::EmptyPath);
        }
        if path.chars().count() > 32_767 {
            return Err(ValidationError::PathTooLong);
        }
        Ok(Self {
            root: PathBuf::from(path),
            policy: ProcessingPolicy {
                quality: JpegQuality::new(quality)?,
                workers: WorkerCount::new(workers)?,
                verify_hash,
                process_mpo,
                delete_thumbs,
            },
        })
    }
}

pub struct AppController {
    paths: AppPaths,
    logger: Arc<FileActivityLog>,
    current_job: Mutex<Option<RunningJob>>,
}

struct RunningJob {
    cancelled: Arc<AtomicBool>,
    /// Set right before the final event is sent, so a run requested in
    /// response to that event joins this thread instead of being rejected.
    completed: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl AppController {
    pub fn new(paths: AppPaths) -> Result<Self, AppError> {
        ImageDatabase::initialize(&paths.database).map_err(AppError::Database)?;
        let logger = FileActivityLog::open(&paths.log).map_err(AppError::Log)?;
        Ok(Self {
            paths,
            logger: Arc::new(logger),
            current_job: Mutex::new(None),
        })
    }

    pub fn start(&self, request: RunRequest, sink: Arc<dyn EventSink>) -> Result<(), AppError> {
        let mut current = self
            .current_job
            .lock()
            .map_err(|_| AppError::StateUnavailable)?;
        if current
            .as_ref()
            .is_some_and(|job| job.completed.load(Ordering::Acquire) || job.handle.is_finished())
            && let Some(finished) = current.take()
        {
            join_job(finished.handle)?;
        }
        if current.is_some() {
            return Err(AppError::AlreadyRunning);
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_completed = Arc::clone(&completed);
        let paths = self.paths.clone();
        let logger = Arc::clone(&self.logger);
        let handle = thread::Builder::new()
            .name("compression-coordinator".to_owned())
            .spawn(move || {
                let finished = run_compression(request, paths, logger, worker_cancelled, &sink);
                worker_completed.store(true, Ordering::Release);
                sink.send(finished);
            })
            .map_err(AppError::Spawn)?;
        *current = Some(RunningJob {
            cancelled,
            completed,
            handle,
        });
        Ok(())
    }

    pub fn cancel(&self) -> Result<bool, AppError> {
        let current = self
            .current_job
            .lock()
            .map_err(|_| AppError::StateUnavailable)?;
        if let Some(job) = current.as_ref() {
            job.cancelled.store(true, Ordering::Relaxed);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn duplicate_report(&self) -> Result<DuplicateReport, AppError> {
        load_duplicate_report(&self.paths.database).map_err(AppError::Database)
    }

    pub fn data_directory(&self) -> &Path {
        &self.paths.data_dir
    }

    pub fn default_worker_count() -> i32 {
        let available = thread::available_parallelism().map_or(4, std::num::NonZero::get);
        i32::try_from(available.clamp(1, 4)).unwrap_or(4)
    }

    pub fn shutdown(&self) -> Result<(), AppError> {
        let job = {
            let mut current = self
                .current_job
                .lock()
                .map_err(|_| AppError::StateUnavailable)?;
            current.take()
        };
        if let Some(job) = job {
            job.cancelled.store(true, Ordering::Relaxed);
            join_job(job.handle)?;
        }
        Ok(())
    }
}

/// Runs one job and returns its final [`AppEvent::Finished`] event, which the
/// caller sends once the job is marked as completed.
fn run_compression(
    request: RunRequest,
    paths: AppPaths,
    logger: Arc<FileActivityLog>,
    cancelled: Arc<AtomicBool>,
    sink: &Arc<dyn EventSink>,
) -> AppEvent {
    let started = Instant::now();
    sink.send(AppEvent::State {
        state: RunState::Scanning,
        status_code: "scanning",
        status_value: 0,
    });
    logger.write(LogLevel::Info, "run", "compression run started");

    let canonical_root = match validate_root(&request.root) {
        Ok(root) => root,
        Err(error) => {
            return fail_run(
                sink,
                &logger,
                error.status_code(),
                &error.to_string(),
                started,
            );
        }
    };
    let scan = match scan_directory(&canonical_root, &cancelled) {
        Ok(scan) => scan,
        Err(ScanError::Cancelled) => return finish_cancelled(RunStats::default(), started),
        Err(error) => {
            return fail_run(sink, &logger, "scan-failed", &error.to_string(), started);
        }
    };

    let mut initial_activity = Vec::new();
    for warning in scan.warnings {
        logger.write(LogLevel::Warning, "scan", &warning.diagnostic);
        initial_activity.push(ActivityEntry {
            level: LogLevel::Warning,
            source: ActivitySource::Scan,
            message_code: warning.code,
            message_args: [0; 3],
        });
    }
    if !initial_activity.is_empty() {
        sink.send(AppEvent::Activity(initial_activity));
    }

    let mut stats = RunStats {
        total_found: scan.images.len(),
        ..RunStats::default()
    };
    sink.send(AppEvent::Progress {
        stats,
        current_file: String::new(),
    });

    if cancelled.load(Ordering::Relaxed) {
        return finish_cancelled(stats, started);
    }

    if !scan.images.is_empty() {
        sink.send(AppEvent::State {
            state: RunState::Processing,
            status_code: "processing",
            status_value: i32::try_from(scan.images.len()).unwrap_or(i32::MAX),
        });
        process_in_parallel(
            &canonical_root,
            &paths.database,
            request.policy,
            scan.images,
            &cancelled,
            &logger,
            sink,
            &mut stats,
        );
    }

    if !cancelled.load(Ordering::Relaxed) && request.policy.delete_thumbs {
        let cleanup = cleanup_thumbs_databases(&canonical_root, &scan.thumbs_databases, &cancelled);
        stats.thumbs_deleted = cleanup.deleted;
        stats.thumbs_errors = cleanup.errors;
        logger.write(
            if cleanup.errors == 0 {
                LogLevel::Info
            } else {
                LogLevel::Warning
            },
            "thumbs-cleanup",
            &format!("deleted={}, errors={}", cleanup.deleted, cleanup.errors),
        );
    }

    if cancelled.load(Ordering::Relaxed) {
        return finish_cancelled(stats, started);
    }

    logger.write(
        if stats.has_incidents() {
            LogLevel::Warning
        } else {
            LogLevel::Info
        },
        "run-summary",
        &format!(
            "found={}, optimized={}, skipped={}, image_errors={}, thumbs_errors={}, partial={}",
            stats.total_found,
            stats.optimized,
            stats.skipped,
            stats.errors,
            stats.thumbs_errors,
            stats.partial
        ),
    );
    AppEvent::Finished {
        state: RunState::Completed,
        stats,
        elapsed: started.elapsed(),
    }
}

#[allow(clippy::too_many_arguments)]
fn process_in_parallel(
    canonical_root: &Path,
    database_path: &Path,
    policy: ProcessingPolicy,
    images: Vec<PathBuf>,
    cancelled: &Arc<AtomicBool>,
    logger: &Arc<FileActivityLog>,
    sink: &Arc<dyn EventSink>,
    stats: &mut RunStats,
) {
    let capacity = policy
        .workers
        .get()
        .saturating_mul(CHANNEL_ITEMS_PER_WORKER)
        .max(1);
    let (task_sender, task_receiver) = bounded::<PathBuf>(capacity);
    let (result_sender, result_receiver) = bounded::<WorkerMessage>(capacity);
    let mut workers = Vec::with_capacity(policy.workers.get());

    for index in 0..policy.workers.get() {
        let task_receiver = task_receiver.clone();
        let result_sender = result_sender.clone();
        let root = canonical_root.to_path_buf();
        let database = database_path.to_path_buf();
        let cancelled = Arc::clone(cancelled);
        match thread::Builder::new()
            .name(format!("image-worker-{index}"))
            .spawn(move || {
                worker_loop(
                    root,
                    database,
                    policy,
                    cancelled,
                    task_receiver,
                    result_sender,
                );
            }) {
            Ok(worker) => workers.push(worker),
            Err(error) => logger.write(
                LogLevel::Error,
                "worker-spawn",
                &format!("a worker could not be started: {error}"),
            ),
        }
    }
    drop(task_receiver);
    drop(result_sender);

    if workers.is_empty() {
        stats.completed = stats.total_found;
        stats.errors = stats.total_found;
        sink.send(AppEvent::Activity(vec![ActivityEntry {
            level: LogLevel::Error,
            source: ActivitySource::Process,
            message_code: "no-workers",
            message_args: [0; 3],
        }]));
        return;
    }

    let producer_cancelled = Arc::clone(cancelled);
    let producer = match thread::Builder::new()
        .name("image-task-producer".to_owned())
        .spawn(move || produce_tasks(images, task_sender, producer_cancelled))
    {
        Ok(producer) => producer,
        Err(error) => {
            logger.write(
                LogLevel::Error,
                "producer-spawn",
                &format!("the task producer could not be started: {error}"),
            );
            join_workers(workers, logger);
            stats.completed = stats.total_found;
            stats.errors = stats.total_found;
            return;
        }
    };

    consume_results(&result_receiver, canonical_root, logger, sink, stats);

    if producer.join().is_err() {
        logger.write(
            LogLevel::Error,
            "producer-join",
            "the task producer stopped unexpectedly",
        );
    }
    join_workers(workers, logger);

    if !cancelled.load(Ordering::Relaxed) && stats.completed < stats.total_found {
        let missing = stats.total_found - stats.completed;
        stats.completed = stats.total_found;
        stats.errors = stats.errors.saturating_add(missing);
    }
}

fn join_workers(workers: Vec<JoinHandle<()>>, logger: &FileActivityLog) {
    for worker in workers {
        if worker.join().is_err() {
            logger.write(
                LogLevel::Error,
                "worker-join",
                "a worker stopped unexpectedly",
            );
        }
    }
}

fn worker_loop(
    canonical_root: PathBuf,
    database_path: PathBuf,
    policy: ProcessingPolicy,
    cancelled: Arc<AtomicBool>,
    tasks: Receiver<PathBuf>,
    results: Sender<WorkerMessage>,
) {
    let mut database = match ImageDatabase::open(&database_path) {
        Ok(database) => database,
        Err(error) => {
            if results
                .send(WorkerMessage::StartupFailure(error.to_string()))
                .is_err()
            {
                eprintln!("the worker SQLite failure could not be reported");
            }
            return;
        }
    };

    while !cancelled.load(Ordering::Relaxed) {
        let path = match tasks.recv_timeout(Duration::from_millis(100)) {
            Ok(path) => path,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        };
        let processed = process_image(&canonical_root, &path, policy, &mut database, &cancelled);
        if results
            .send(WorkerMessage::File { path, processed })
            .is_err()
        {
            break;
        }
    }
}

fn produce_tasks(images: Vec<PathBuf>, sender: Sender<PathBuf>, cancelled: Arc<AtomicBool>) {
    for image in images {
        if cancelled.load(Ordering::Relaxed) || sender.send(image).is_err() {
            break;
        }
    }
}

fn consume_results(
    receiver: &Receiver<WorkerMessage>,
    canonical_root: &Path,
    logger: &Arc<FileActivityLog>,
    sink: &Arc<dyn EventSink>,
    stats: &mut RunStats,
) {
    let mut pending = Vec::with_capacity(UI_BATCH_SIZE);
    let mut last_flush = Instant::now();
    let mut current_file = String::new();
    while let Ok(message) = receiver.recv() {
        match message {
            WorkerMessage::File { path, processed } => {
                current_file = relative_label(canonical_root, &path);
                stats.register(&processed.outcome);
                let diagnostic_message = processed
                    .diagnostic
                    .as_ref()
                    .map(|diagnostic| format!("{} ({diagnostic})", processed.summary));
                let log_message = diagnostic_message.as_deref().unwrap_or(&processed.summary);
                logger.write(
                    processed.level,
                    "image",
                    &format!("{}: {log_message}", current_file),
                );
                if !matches!(processed.outcome, ImageOutcome::Cancelled) {
                    pending.push(ActivityEntry {
                        level: processed.level,
                        source: ActivitySource::Image(current_file.clone()),
                        message_code: processed.message_code,
                        message_args: processed.message_args,
                    });
                }
            }
            WorkerMessage::StartupFailure(diagnostic) => {
                logger.write(LogLevel::Error, "database-open", &diagnostic);
                pending.push(ActivityEntry {
                    level: LogLevel::Error,
                    source: ActivitySource::Database,
                    message_code: "worker-database-unavailable",
                    message_args: [0; 3],
                });
            }
        }

        if pending.len() >= UI_BATCH_SIZE || last_flush.elapsed() >= UI_BATCH_INTERVAL {
            flush_ui_batch(sink, stats, &current_file, &mut pending);
            last_flush = Instant::now();
        }
    }
    flush_ui_batch(sink, stats, &current_file, &mut pending);
}

fn flush_ui_batch(
    sink: &Arc<dyn EventSink>,
    stats: &RunStats,
    current_file: &str,
    pending: &mut Vec<ActivityEntry>,
) {
    if !pending.is_empty() {
        sink.send(AppEvent::Activity(std::mem::take(pending)));
    }
    sink.send(AppEvent::Progress {
        stats: *stats,
        current_file: current_file.to_owned(),
    });
}

fn validate_root(root: &Path) -> Result<PathBuf, RootError> {
    let metadata = fs::metadata(root).map_err(RootError::Metadata)?;
    if !metadata.is_dir() {
        return Err(RootError::NotDirectory);
    }
    root.canonicalize().map_err(RootError::Canonicalize)
}

fn relative_label(root: &Path, path: &Path) -> String {
    let value = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
    value.chars().take(240).collect()
}

fn fail_run(
    sink: &Arc<dyn EventSink>,
    logger: &Arc<FileActivityLog>,
    status_code: &'static str,
    diagnostic: &str,
    started: Instant,
) -> AppEvent {
    logger.write(LogLevel::Error, "run-failed", diagnostic);
    sink.send(AppEvent::State {
        state: RunState::Failed,
        status_code,
        status_value: 0,
    });
    AppEvent::Finished {
        state: RunState::Failed,
        stats: RunStats::default(),
        elapsed: started.elapsed(),
    }
}

fn finish_cancelled(stats: RunStats, started: Instant) -> AppEvent {
    AppEvent::Finished {
        state: RunState::Cancelled,
        stats,
        elapsed: started.elapsed(),
    }
}

fn join_job(handle: JoinHandle<()>) -> Result<(), AppError> {
    handle.join().map_err(|_| AppError::ThreadPanicked)
}

enum WorkerMessage {
    File {
        path: PathBuf,
        processed: ProcessedFile,
    },
    StartupFailure(String),
}

#[derive(Debug)]
enum RootError {
    Metadata(std::io::Error),
    NotDirectory,
    Canonicalize(std::io::Error),
}

impl RootError {
    fn status_code(&self) -> &'static str {
        match self {
            Self::Metadata(_) => "root-unavailable",
            Self::NotDirectory => "root-not-directory",
            Self::Canonicalize(_) => "root-invalid",
        }
    }
}

impl fmt::Display for RootError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Metadata(error) => write!(formatter, "root access: {error}"),
            Self::NotDirectory => formatter.write_str("the root is not a directory"),
            Self::Canonicalize(error) => write!(formatter, "canonical root path: {error}"),
        }
    }
}

#[derive(Debug)]
pub enum AppError {
    Database(DatabaseError),
    Log(std::io::Error),
    Spawn(std::io::Error),
    StateUnavailable,
    AlreadyRunning,
    ThreadPanicked,
}

impl AppError {
    pub fn status_code(&self) -> &'static str {
        match self {
            Self::Database(_) => "database-unavailable",
            Self::Log(_) => "log-unavailable",
            Self::Spawn(_) => "background-start-failed",
            Self::StateUnavailable => "state-unavailable",
            Self::AlreadyRunning => "already-running",
            Self::ThreadPanicked => "thread-panicked",
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "database: {error}"),
            Self::Log(error) => write!(formatter, "activity log: {error}"),
            Self::Spawn(error) => write!(formatter, "thread startup: {error}"),
            Self::StateUnavailable => formatter.write_str("poisoned state lock"),
            Self::AlreadyRunning => formatter.write_str("a run is already active"),
            Self::ThreadPanicked => formatter.write_str("a thread ended with a panic"),
        }
    }
}

impl std::error::Error for AppError {}

#[cfg(test)]
mod tests {
    use super::RunRequest;

    #[test]
    fn request_validates_empty_paths_and_numeric_bounds() {
        assert!(RunRequest::new("", 50, 4, true, true, false).is_err());
        assert!(RunRequest::new("images", 0, 4, true, true, false).is_err());
        assert!(RunRequest::new("images", 50, 0, true, true, false).is_err());
        assert!(RunRequest::new("images", 50, 4, true, true, false).is_ok());
    }

    #[test]
    fn request_ignores_surrounding_whitespace_in_the_path() {
        assert!(RunRequest::new("   ", 50, 4, true, true, false).is_err());
        let request = RunRequest::new("  images \t", 50, 4, true, true, false);
        assert_eq!(
            request.map(|request| request.root),
            Ok(std::path::PathBuf::from("images"))
        );
    }
}
