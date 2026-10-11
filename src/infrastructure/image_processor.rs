use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use image::codecs::jpeg::JpegEncoder;
use image::metadata::Orientation;
use image::{
    DynamicImage, ImageDecoder, ImageEncoder, ImageError, ImageFormat, ImageReader, Limits,
};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::domain::{ImageOutcome, ProcessingPolicy, SkipReason};
use crate::ports::{ImageRegistry, LogLevel, RegistryError};

const MAX_INPUT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 20_000;
const MAX_IMAGE_PIXELS: u64 = 40_000_000;
const MAX_DECODE_ALLOCATION: u64 = 192 * 1024 * 1024;
const MPO_HEADER_SCAN_BYTES: u64 = 1024 * 1024;

#[derive(Debug)]
pub struct ProcessedFile {
    pub outcome: ImageOutcome,
    pub level: LogLevel,
    pub summary: String,
    pub message_code: &'static str,
    pub message_args: [i32; 3],
    pub diagnostic: Option<String>,
}

impl ProcessedFile {
    fn simple(
        outcome: ImageOutcome,
        level: LogLevel,
        summary: impl Into<String>,
        message_code: &'static str,
    ) -> Self {
        Self::new(outcome, level, summary, message_code, [0; 3], None)
    }

    fn with_diagnostic(
        outcome: ImageOutcome,
        level: LogLevel,
        summary: impl Into<String>,
        message_code: &'static str,
        diagnostic: impl Into<String>,
    ) -> Self {
        Self::new(
            outcome,
            level,
            summary,
            message_code,
            [0; 3],
            Some(diagnostic.into()),
        )
    }

    fn new(
        outcome: ImageOutcome,
        level: LogLevel,
        summary: impl Into<String>,
        message_code: &'static str,
        message_args: [i32; 3],
        diagnostic: Option<String>,
    ) -> Self {
        Self {
            outcome,
            level,
            summary: summary.into(),
            message_code,
            message_args,
            diagnostic,
        }
    }
}

pub fn process_image<R: ImageRegistry>(
    canonical_root: &Path,
    path: &Path,
    policy: ProcessingPolicy,
    registry: &mut R,
    cancelled: &AtomicBool,
) -> ProcessedFile {
    match process_image_inner(canonical_root, path, policy, registry, cancelled) {
        Ok(file) => file,
        Err(ProcessError::Cancelled) => ProcessedFile::simple(
            ImageOutcome::Cancelled,
            LogLevel::Info,
            "cancellation confirmed before replacing the file",
            "cancelled",
        ),
        Err(ProcessError::MpoDisabled) => ProcessedFile::simple(
            ImageOutcome::Skipped(SkipReason::MpoDisabled),
            LogLevel::Warning,
            "MPO skipped because MPO processing is disabled",
            "mpo-disabled",
        ),
        Err(ProcessError::UnsupportedFormat) => ProcessedFile::simple(
            ImageOutcome::Skipped(SkipReason::UnsupportedFormat),
            LogLevel::Warning,
            "file skipped because its content has an unsupported format",
            "unsupported-format",
        ),
        Err(ProcessError::ResourceLimit(message)) => ProcessedFile::with_diagnostic(
            ImageOutcome::Skipped(SkipReason::ResourceLimit),
            LogLevel::Warning,
            "file skipped because it exceeds a safety limit",
            "resource-limit",
            message,
        ),
        Err(ProcessError::Registry(error)) => ProcessedFile::with_diagnostic(
            ImageOutcome::Failed,
            LogLevel::Error,
            "the registry could not be queried; the file was not changed",
            "registry-unavailable",
            error.to_string(),
        ),
        Err(error) => {
            let summary = error.user_message();
            let message_code = error.message_code();
            ProcessedFile::with_diagnostic(
                ImageOutcome::Failed,
                LogLevel::Error,
                summary,
                message_code,
                error.to_string(),
            )
        }
    }
}

fn process_image_inner<R: ImageRegistry>(
    canonical_root: &Path,
    path: &Path,
    policy: ProcessingPolicy,
    registry: &mut R,
    cancelled: &AtomicBool,
) -> Result<ProcessedFile, ProcessError> {
    check_cancelled(cancelled)?;
    let metadata = fs::symlink_metadata(path).map_err(ProcessError::Metadata)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ProcessError::PathEscaped);
    }
    // A read-only original cannot be replaced, and on Windows a read-only
    // temporary copy could not even be cleaned up afterwards.
    if metadata.permissions().readonly() {
        return Err(ProcessError::ReadOnly);
    }
    if metadata.len() > MAX_INPUT_BYTES {
        return Err(ProcessError::ResourceLimit(format!(
            "the file exceeds {} MiB",
            MAX_INPUT_BYTES / 1024 / 1024
        )));
    }

    let canonical_path = path.canonicalize().map_err(ProcessError::Canonicalize)?;
    if !canonical_path.starts_with(canonical_root) {
        return Err(ProcessError::PathEscaped);
    }

    let current_hash = generate_hash(&canonical_path, Some(cancelled))?;
    check_cancelled(cancelled)?;
    if policy.verify_hash
        && let Some(processed) = registry
            .processed_image(&current_hash, &canonical_path)
            .map_err(ProcessError::Registry)?
    {
        return Ok(ProcessedFile::new(
            ImageOutcome::Skipped(SkipReason::AlreadyOptimized {
                duplicate_count: processed.duplicate_count,
            }),
            LogLevel::Info,
            if processed.duplicate_count > 1 {
                format!(
                    "already optimized; {} copies have the same content",
                    processed.duplicate_count
                )
            } else {
                "already optimized".to_owned()
            },
            if processed.duplicate_count > 1 {
                "already-optimized-duplicates"
            } else {
                "already-optimized"
            },
            [
                i32::try_from(processed.duplicate_count).unwrap_or(i32::MAX),
                0,
                0,
            ],
            None,
        ));
    }

    let mpo = is_mpo(&canonical_path).map_err(ProcessError::ReadMpo)?;
    if mpo && !policy.process_mpo {
        return Err(ProcessError::MpoDisabled);
    }
    check_cancelled(cancelled)?;

    let reader = ImageReader::open(&canonical_path)
        .map_err(ProcessError::OpenImage)?
        .with_guessed_format()
        .map_err(ProcessError::GuessFormat)?;
    let format = reader.format().ok_or(ProcessError::UnsupportedFormat)?;
    if !matches!(
        format,
        ImageFormat::Jpeg
            | ImageFormat::Png
            | ImageFormat::Bmp
            | ImageFormat::Tiff
            | ImageFormat::WebP
    ) {
        return Err(ProcessError::UnsupportedFormat);
    }
    let (width, height) = reader.into_dimensions().map_err(ProcessError::Dimensions)?;
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION || pixels > MAX_IMAGE_PIXELS {
        return Err(ProcessError::ResourceLimit(format!(
            "dimensions {width}×{height}; maximum {MAX_IMAGE_PIXELS} pixels"
        )));
    }

    let mut reader = ImageReader::open(&canonical_path)
        .map_err(ProcessError::OpenImage)?
        .with_guessed_format()
        .map_err(ProcessError::GuessFormat)?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOCATION);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(decode_error)?;
    // The re-encoded file carries no EXIF block, so the orientation is applied
    // to the pixels; otherwise rotated camera photos would display sideways.
    // Unreadable metadata is treated as absent rather than failing the image.
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let icc_profile = decoder.icc_profile().ok().flatten();
    let mut decoded = DynamicImage::from_decoder(decoder).map_err(decode_error)?;
    decoded.apply_orientation(orientation);
    check_cancelled(cancelled)?;

    let rgb = decoded.into_rgb8();
    let parent = canonical_path.parent().ok_or(ProcessError::MissingParent)?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(ProcessError::CreateTemporary)?;
    {
        let mut encoder =
            JpegEncoder::new_with_quality(temporary.as_file_mut(), policy.quality.get());
        if let Some(icc_profile) = icc_profile {
            encoder
                .set_icc_profile(icc_profile)
                .map_err(|error| ProcessError::Encode(ImageError::Unsupported(error)))?;
        }
        encoder.encode_image(&rgb).map_err(ProcessError::Encode)?;
    }
    temporary
        .as_file_mut()
        .flush()
        .map_err(ProcessError::FlushTemporary)?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(ProcessError::SyncTemporary)?;
    fs::set_permissions(temporary.path(), metadata.permissions())
        .map_err(ProcessError::SetPermissions)?;
    check_cancelled(cancelled)?;

    let before = metadata.len();
    let after = temporary
        .as_file()
        .metadata()
        .map_err(ProcessError::TemporaryMetadata)?
        .len();
    if format == ImageFormat::Jpeg && !mpo && after >= before {
        // Re-encoding would only lose quality without saving space. The
        // temporary file is discarded on return and the original is recorded
        // so the next run skips it.
        let recorded = registry.record_image(&current_hash, &canonical_path, policy.quality.get());
        let (level, diagnostic) = match recorded {
            Ok(()) => (LogLevel::Info, None),
            Err(error) => (LogLevel::Warning, Some(error.to_string())),
        };
        return Ok(ProcessedFile::new(
            ImageOutcome::Skipped(SkipReason::NotSmaller),
            level,
            format!(
                "kept the original; re-encoding at {}% would not reduce it ({} → {} KiB)",
                policy.quality.get(),
                before / 1024,
                after / 1024
            ),
            "not-smaller",
            kib_args(policy.quality.get(), before, after),
            diagnostic,
        ));
    }
    let persisted = temporary
        .persist(&canonical_path)
        .map_err(|error| ProcessError::Persist(error.error))?;
    if let Err(error) = persisted.sync_all() {
        return Ok(ProcessedFile::with_diagnostic(
            ImageOutcome::Partial { before, after },
            LogLevel::Error,
            "the file was replaced, but persistence could not be confirmed",
            "persist-verification-failed",
            format!("final sync: {error}"),
        ));
    }

    let new_hash = match generate_hash(&canonical_path, None) {
        Ok(hash) => hash,
        Err(error) => {
            return Ok(ProcessedFile::with_diagnostic(
                ImageOutcome::Partial { before, after },
                LogLevel::Error,
                "optimized, but the final hash could not be calculated",
                "final-hash-failed",
                error.to_string(),
            ));
        }
    };
    if let Err(error) = registry.record_image(&new_hash, &canonical_path, policy.quality.get()) {
        return Ok(ProcessedFile::with_diagnostic(
            ImageOutcome::Partial { before, after },
            LogLevel::Error,
            "optimized, but it could not be recorded in the database",
            "database-record-failed",
            error.to_string(),
        ));
    }

    Ok(ProcessedFile::new(
        ImageOutcome::Success { before, after },
        LogLevel::Info,
        format!(
            "optimized at {}% ({} → {} KiB)",
            policy.quality.get(),
            before / 1024,
            after / 1024
        ),
        "optimized",
        kib_args(policy.quality.get(), before, after),
        None,
    ))
}

fn kib_args(quality: u8, before: u64, after: u64) -> [i32; 3] {
    [
        i32::from(quality),
        i32::try_from(before / 1024).unwrap_or(i32::MAX),
        i32::try_from(after / 1024).unwrap_or(i32::MAX),
    ]
}

fn decode_error(error: ImageError) -> ProcessError {
    match error {
        ImageError::Limits(error) => ProcessError::ResourceLimit(error.to_string()),
        error => ProcessError::Decode(error),
    }
}

fn generate_hash(path: &Path, cancelled: Option<&AtomicBool>) -> Result<String, ProcessError> {
    let file = File::open(path).map_err(ProcessError::Hash)?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if let Some(cancelled) = cancelled {
            check_cancelled(cancelled)?;
        }
        let count = reader.read(&mut buffer).map_err(ProcessError::Hash)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest.iter().copied() {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(encoded)
}

fn is_mpo(path: &Path) -> io::Result<bool> {
    let file = File::open(path)?;
    let mut header = Vec::with_capacity(MPO_HEADER_SCAN_BYTES as usize);
    file.take(MPO_HEADER_SCAN_BYTES).read_to_end(&mut header)?;
    Ok(header.windows(4).any(|window| window == b"MPF\0"))
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), ProcessError> {
    if cancelled.load(Ordering::Relaxed) {
        Err(ProcessError::Cancelled)
    } else {
        Ok(())
    }
}

#[derive(Debug)]
enum ProcessError {
    Cancelled,
    Metadata(io::Error),
    Canonicalize(io::Error),
    PathEscaped,
    ReadOnly,
    ResourceLimit(String),
    Hash(io::Error),
    Registry(RegistryError),
    ReadMpo(io::Error),
    MpoDisabled,
    OpenImage(io::Error),
    GuessFormat(io::Error),
    UnsupportedFormat,
    Dimensions(ImageError),
    Decode(ImageError),
    MissingParent,
    CreateTemporary(io::Error),
    Encode(ImageError),
    FlushTemporary(io::Error),
    SyncTemporary(io::Error),
    TemporaryMetadata(io::Error),
    SetPermissions(io::Error),
    Persist(io::Error),
}

impl ProcessError {
    fn message_code(&self) -> &'static str {
        match self {
            Self::Metadata(_) | Self::Canonicalize(_) | Self::Hash(_) | Self::ReadMpo(_) => {
                "read-failed"
            }
            Self::PathEscaped => "path-escaped",
            Self::ReadOnly => "read-only",
            Self::OpenImage(_) | Self::GuessFormat(_) | Self::Dimensions(_) | Self::Decode(_) => {
                "decode-failed"
            }
            Self::MissingParent | Self::CreateTemporary(_) => "temporary-create-failed",
            Self::Encode(_)
            | Self::FlushTemporary(_)
            | Self::SyncTemporary(_)
            | Self::TemporaryMetadata(_) => "temporary-write-failed",
            Self::SetPermissions(_) | Self::Persist(_) => "replace-failed",
            Self::Cancelled => "cancelled",
            Self::ResourceLimit(_) => "resource-limit",
            Self::Registry(_) => "registry-unavailable",
            Self::MpoDisabled => "mpo-disabled",
            Self::UnsupportedFormat => "unsupported-format",
        }
    }

    fn user_message(&self) -> &'static str {
        match self {
            Self::Metadata(_) | Self::Canonicalize(_) | Self::Hash(_) | Self::ReadMpo(_) => {
                "the file could not be read; it was not changed"
            }
            Self::PathEscaped => "file skipped because it is no longer inside the selected folder",
            Self::ReadOnly => "the file is read-only; it was not changed",
            Self::OpenImage(_) | Self::GuessFormat(_) | Self::Dimensions(_) | Self::Decode(_) => {
                "the image could not be decoded; it was not changed"
            }
            Self::MissingParent | Self::CreateTemporary(_) => {
                "the temporary file could not be created; the original remains intact"
            }
            Self::Encode(_)
            | Self::FlushTemporary(_)
            | Self::SyncTemporary(_)
            | Self::TemporaryMetadata(_) => {
                "the temporary image could not be written; the original remains intact"
            }
            Self::SetPermissions(_) | Self::Persist(_) => {
                "the file could not be replaced; the original remains intact"
            }
            Self::Cancelled
            | Self::ResourceLimit(_)
            | Self::Registry(_)
            | Self::MpoDisabled
            | Self::UnsupportedFormat => "the file was not processed",
        }
    }
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("operation cancelled"),
            Self::Metadata(error) => write!(formatter, "metadata: {error}"),
            Self::Canonicalize(error) => write!(formatter, "canonical path: {error}"),
            Self::PathEscaped => formatter.write_str("the path escaped the authorized root"),
            Self::ReadOnly => formatter.write_str("read-only file"),
            Self::ResourceLimit(message) => formatter.write_str(message),
            Self::Hash(error) => write!(formatter, "hash: {error}"),
            Self::Registry(error) => write!(formatter, "registry: {error}"),
            Self::ReadMpo(error) => write!(formatter, "MPO header: {error}"),
            Self::MpoDisabled => formatter.write_str("MPO processing disabled"),
            Self::OpenImage(error) => write!(formatter, "open: {error}"),
            Self::GuessFormat(error) => write!(formatter, "format detection: {error}"),
            Self::UnsupportedFormat => formatter.write_str("unsupported format"),
            Self::Dimensions(error) => write!(formatter, "dimensions: {error}"),
            Self::Decode(error) => write!(formatter, "decode: {error}"),
            Self::MissingParent => formatter.write_str("file has no parent directory"),
            Self::CreateTemporary(error) => write!(formatter, "temporary file: {error}"),
            Self::Encode(error) => write!(formatter, "encode: {error}"),
            Self::FlushTemporary(error) => write!(formatter, "temporary flush: {error}"),
            Self::SyncTemporary(error) => write!(formatter, "temporary sync: {error}"),
            Self::TemporaryMetadata(error) => write!(formatter, "temporary metadata: {error}"),
            Self::SetPermissions(error) => write!(formatter, "permissions: {error}"),
            Self::Persist(error) => write!(formatter, "atomic replacement: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs::File;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;

    use image::codecs::jpeg::JpegEncoder;
    use image::{ImageEncoder, Rgb, RgbImage};
    use tempfile::tempdir;

    use super::process_image;
    use crate::domain::{ImageOutcome, JpegQuality, ProcessingPolicy, SkipReason, WorkerCount};
    use crate::ports::{ImageRegistry, ProcessedImage, RegistryError};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[derive(Default)]
    struct MemoryRegistry {
        hashes: HashMap<PathBuf, String>,
    }

    impl ImageRegistry for MemoryRegistry {
        fn processed_image(
            &mut self,
            hash: &str,
            path: &Path,
        ) -> Result<Option<ProcessedImage>, RegistryError> {
            Ok(self
                .hashes
                .get(path)
                .filter(|stored| *stored == hash)
                .map(|_| ProcessedImage { duplicate_count: 1 }))
        }

        fn record_image(
            &mut self,
            hash: &str,
            path: &Path,
            _quality: u8,
        ) -> Result<(), RegistryError> {
            self.hashes.insert(path.to_path_buf(), hash.to_owned());
            Ok(())
        }
    }

    fn policy(quality: i32) -> Result<ProcessingPolicy, Box<dyn std::error::Error>> {
        Ok(ProcessingPolicy {
            quality: JpegQuality::new(quality)?,
            workers: WorkerCount::new(2)?,
            verify_hash: true,
            process_mpo: true,
            delete_thumbs: false,
        })
    }

    /// Writes a JPEG with deterministic noise, so its size reacts to quality.
    fn write_noisy_jpeg(
        path: &Path,
        width: u32,
        height: u32,
        quality: u8,
        exif: Option<Vec<u8>>,
    ) -> TestResult {
        let mut state = 0x2545_f491_u32;
        let image = RgbImage::from_fn(width, height, |_, _| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let [red, green, blue, _] = state.to_le_bytes();
            Rgb([red, green, blue])
        });
        let mut encoder = JpegEncoder::new_with_quality(File::create(path)?, quality);
        if let Some(exif) = exif {
            encoder.set_exif_metadata(exif)?;
        }
        encoder.encode_image(&image)?;
        Ok(())
    }

    /// A little-endian TIFF block holding only the EXIF orientation tag.
    fn exif_orientation(value: u16) -> Vec<u8> {
        let mut exif = b"II*\0".to_vec();
        exif.extend_from_slice(&8_u32.to_le_bytes());
        exif.extend_from_slice(&1_u16.to_le_bytes());
        exif.extend_from_slice(&0x0112_u16.to_le_bytes());
        exif.extend_from_slice(&3_u16.to_le_bytes());
        exif.extend_from_slice(&1_u32.to_le_bytes());
        exif.extend_from_slice(&value.to_le_bytes());
        exif.extend_from_slice(&[0, 0]);
        exif.extend_from_slice(&0_u32.to_le_bytes());
        exif
    }

    #[test]
    fn jpeg_is_replaced_and_second_run_is_idempotent() -> TestResult {
        let directory = tempdir()?;
        let path = directory.path().join("sample.jpg");
        write_noisy_jpeg(&path, 64, 64, 100, None)?;
        let root = directory.path().canonicalize()?;
        let mut registry = MemoryRegistry::default();

        let first = process_image(
            &root,
            &path,
            policy(60)?,
            &mut registry,
            &AtomicBool::new(false),
        );
        let second = process_image(
            &root,
            &path,
            policy(60)?,
            &mut registry,
            &AtomicBool::new(false),
        );

        assert!(matches!(first.outcome, ImageOutcome::Success { .. }));
        assert!(matches!(
            second.outcome,
            ImageOutcome::Skipped(SkipReason::AlreadyOptimized { .. })
        ));
        Ok(())
    }

    #[test]
    fn exif_orientation_is_applied_to_the_pixels() -> TestResult {
        let directory = tempdir()?;
        let path = directory.path().join("rotated.jpg");
        // Orientation 6 means "rotate 90° clockwise to display".
        write_noisy_jpeg(&path, 48, 16, 100, Some(exif_orientation(6)))?;
        let root = directory.path().canonicalize()?;

        let result = process_image(
            &root,
            &path,
            policy(60)?,
            &mut MemoryRegistry::default(),
            &AtomicBool::new(false),
        );

        assert!(matches!(result.outcome, ImageOutcome::Success { .. }));
        assert_eq!(image::image_dimensions(&path)?, (16, 48));
        Ok(())
    }

    #[test]
    fn jpeg_that_would_grow_is_kept_and_recorded() -> TestResult {
        let directory = tempdir()?;
        let path = directory.path().join("small.jpg");
        write_noisy_jpeg(&path, 64, 64, 20, None)?;
        let before = std::fs::read(&path)?;
        let root = directory.path().canonicalize()?;
        let mut registry = MemoryRegistry::default();

        let first = process_image(
            &root,
            &path,
            policy(95)?,
            &mut registry,
            &AtomicBool::new(false),
        );
        let second = process_image(
            &root,
            &path,
            policy(95)?,
            &mut registry,
            &AtomicBool::new(false),
        );

        assert_eq!(first.outcome, ImageOutcome::Skipped(SkipReason::NotSmaller));
        assert_eq!(std::fs::read(&path)?, before);
        assert!(matches!(
            second.outcome,
            ImageOutcome::Skipped(SkipReason::AlreadyOptimized { .. })
        ));
        Ok(())
    }

    #[test]
    fn read_only_jpeg_is_left_untouched() -> TestResult {
        let directory = tempdir()?;
        let path = directory.path().join("locked.jpg");
        write_noisy_jpeg(&path, 32, 32, 100, None)?;
        let before = std::fs::read(&path)?;
        let mut permissions = std::fs::metadata(&path)?.permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions.clone())?;
        let root = directory.path().canonicalize()?;

        let result = process_image(
            &root,
            &path,
            policy(60)?,
            &mut MemoryRegistry::default(),
            &AtomicBool::new(false),
        );

        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions)?;
        assert!(matches!(result.outcome, ImageOutcome::Failed));
        assert_eq!(result.message_code, "read-only");
        assert_eq!(std::fs::read(&path)?, before);
        assert_eq!(std::fs::read_dir(directory.path())?.count(), 1);
        Ok(())
    }

    #[test]
    fn corrupt_jpeg_never_replaces_the_input() -> TestResult {
        let directory = tempdir()?;
        let path = directory.path().join("broken.jpg");
        std::fs::write(&path, b"not an image")?;
        let before = std::fs::read(&path)?;
        let root = directory.path().canonicalize()?;

        let result = process_image(
            &root,
            &path,
            policy(60)?,
            &mut MemoryRegistry::default(),
            &AtomicBool::new(false),
        );

        assert!(matches!(result.outcome, ImageOutcome::Failed));
        assert_eq!(std::fs::read(path)?, before);
        Ok(())
    }

    #[test]
    fn cancellation_before_work_preserves_the_input() -> TestResult {
        let directory = tempdir()?;
        let path = directory.path().join("sample.jpg");
        let image = RgbImage::from_pixel(8, 8, Rgb([1, 2, 3]));
        image.save(&path)?;
        let before = std::fs::read(&path)?;
        let root = directory.path().canonicalize()?;

        let result = process_image(
            &root,
            &path,
            policy(60)?,
            &mut MemoryRegistry::default(),
            &AtomicBool::new(true),
        );

        assert!(matches!(result.outcome, ImageOutcome::Cancelled));
        assert_eq!(std::fs::read(path)?, before);
        Ok(())
    }
}
