use std::fs::{self, File};
use std::io::{self, Cursor, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use enough::{Stop, StopReason};
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader, Limits};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use zenjpeg::encoder::{ChromaSubsampling, EncoderConfig, PixelLayout};

use crate::domain::{ImageOutcome, ProcessingPolicy, SkipReason};
use crate::infrastructure::pixel_budget::PixelBudget;
use crate::ports::{ImageRegistry, LogLevel, RegistryError};

/// Each image is read into memory once, so this also bounds the memory a
/// worker holds for the encoded input.
const MAX_INPUT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 20_000;
/// Covers 150-megapixel medium-format cameras and large panoramas.
const MAX_IMAGE_PIXELS: u64 = 250_000_000;
/// The largest image at 16-bit RGBA, the widest pixel format decoded.
const MAX_DECODE_ALLOCATION: u64 = MAX_IMAGE_PIXELS * 8;
/// Pixels decoded and encoded at once across all workers, about 3 GiB.
const PIXELS_IN_FLIGHT: u64 = 500_000_000;
const MPO_HEADER_SCAN_BYTES: usize = 1024 * 1024;

static PIXEL_BUDGET: PixelBudget = PixelBudget::new(PIXELS_IN_FLIGHT);

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

    // The file is read once; hashing, format detection, decoding, and the
    // size comparison all work on this buffer, which matters most on network
    // shares where every extra read or open is a round trip.
    let original = read_limited(&canonical_path)?;
    check_cancelled(cancelled)?;
    let current_hash = sha256_hex(&original);
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

    let mpo = is_mpo(&original);
    if mpo && !policy.process_mpo {
        return Err(ProcessError::MpoDisabled);
    }
    check_cancelled(cancelled)?;

    let reader = image_reader(&original)?;
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

    let lease = PIXEL_BUDGET
        .acquire(pixels, cancelled)
        .ok_or(ProcessError::Cancelled)?;
    let mut reader = image_reader(&original)?;
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

    let encoded = encode_jpeg(
        &decoded.into_rgb8(),
        policy.quality.get(),
        icc_profile.as_deref(),
        cancelled,
    )?;
    drop(lease);
    let before = u64::try_from(original.len()).unwrap_or(u64::MAX);
    let after = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
    if format == ImageFormat::Jpeg && !mpo && after >= before {
        // Re-encoding would only lose quality without saving space. Nothing
        // has been written, and the original is recorded so the next run
        // skips it.
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
    check_cancelled(cancelled)?;

    // One write and one flush to a temporary file on the same volume, then an
    // atomic rename. The rename only updates metadata, so the data is already
    // durable when it happens and no second flush is needed.
    let parent = canonical_path.parent().ok_or(ProcessError::MissingParent)?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(ProcessError::CreateTemporary)?;
    temporary
        .as_file_mut()
        .write_all(&encoded)
        .map_err(ProcessError::WriteTemporary)?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(ProcessError::SyncTemporary)?;
    fs::set_permissions(temporary.path(), metadata.permissions())
        .map_err(ProcessError::SetPermissions)?;
    check_cancelled(cancelled)?;
    temporary
        .persist(&canonical_path)
        .map_err(|error| ProcessError::Persist(error.error))?;

    let new_hash = sha256_hex(&encoded);
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

/// Reads the whole file, refusing one that grew past the limit after the
/// metadata check.
fn read_limited(path: &Path) -> Result<Vec<u8>, ProcessError> {
    let file = File::open(path).map_err(ProcessError::Read)?;
    let expected = file.metadata().map_or(0, |metadata| metadata.len());
    let mut bytes = Vec::with_capacity(usize::try_from(expected.min(MAX_INPUT_BYTES)).unwrap_or(0));
    file.take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(ProcessError::Read)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_INPUT_BYTES {
        return Err(ProcessError::ResourceLimit(format!(
            "the file exceeds {} MiB",
            MAX_INPUT_BYTES / 1024 / 1024
        )));
    }
    Ok(bytes)
}

/// Detects the format from the content. Every scanned file has a JPEG
/// extension, so content without a recognizable signature is decoded as JPEG
/// and reported as a decoding error rather than skipped.
fn image_reader(bytes: &[u8]) -> Result<ImageReader<Cursor<&[u8]>>, ProcessError> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(ProcessError::GuessFormat)?;
    if reader.format().is_none() {
        reader.set_format(ImageFormat::Jpeg);
    }
    Ok(reader)
}

/// Encodes with zenjpeg: 4:2:0 chroma, progressive scans, and optimized
/// Huffman tables. The cancellation flag is checked while rows are encoded.
fn encode_jpeg(
    rgb: &image::RgbImage,
    quality: u8,
    icc_profile: Option<&[u8]>,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, ProcessError> {
    let config = EncoderConfig::ycbcr(quality, ChromaSubsampling::Quarter).progressive(true);
    let mut request = config.request();
    if let Some(icc_profile) = icc_profile {
        request = request.icc_profile(icc_profile);
    }
    let encode = || {
        let mut encoder =
            request.encode_from_bytes(rgb.width(), rgb.height(), PixelLayout::Rgb8Srgb)?;
        encoder.push_packed(rgb.as_raw(), CancelFlag(cancelled))?;
        encoder.finish()
    };
    encode().map_err(|error| {
        if cancelled.load(Ordering::Relaxed) {
            ProcessError::Cancelled
        } else {
            ProcessError::Encode(error.to_string())
        }
    })
}

/// Lets the encoder stop between rows when the run is cancelled.
struct CancelFlag<'a>(&'a AtomicBool);

impl Stop for CancelFlag<'_> {
    fn check(&self) -> Result<(), StopReason> {
        if self.0.load(Ordering::Relaxed) {
            Err(StopReason::Cancelled)
        } else {
            Ok(())
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest.iter().copied() {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

/// MPO files carry an "MPF" APP2 segment near the start of the file.
fn is_mpo(bytes: &[u8]) -> bool {
    let header = &bytes[..bytes.len().min(MPO_HEADER_SCAN_BYTES)];
    header.windows(4).any(|window| window == b"MPF\0")
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
    Read(io::Error),
    Registry(RegistryError),
    MpoDisabled,
    GuessFormat(io::Error),
    UnsupportedFormat,
    Dimensions(ImageError),
    Decode(ImageError),
    MissingParent,
    CreateTemporary(io::Error),
    Encode(String),
    WriteTemporary(io::Error),
    SyncTemporary(io::Error),
    SetPermissions(io::Error),
    Persist(io::Error),
}

impl ProcessError {
    fn message_code(&self) -> &'static str {
        match self {
            Self::Metadata(_) | Self::Canonicalize(_) | Self::Read(_) => "read-failed",
            Self::PathEscaped => "path-escaped",
            Self::ReadOnly => "read-only",
            Self::GuessFormat(_) | Self::Dimensions(_) | Self::Decode(_) => "decode-failed",
            Self::MissingParent | Self::CreateTemporary(_) => "temporary-create-failed",
            Self::Encode(_) | Self::WriteTemporary(_) | Self::SyncTemporary(_) => {
                "temporary-write-failed"
            }
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
            Self::Metadata(_) | Self::Canonicalize(_) | Self::Read(_) => {
                "the file could not be read; it was not changed"
            }
            Self::PathEscaped => "file skipped because it is no longer inside the selected folder",
            Self::ReadOnly => "the file is read-only; it was not changed",
            Self::GuessFormat(_) | Self::Dimensions(_) | Self::Decode(_) => {
                "the image could not be decoded; it was not changed"
            }
            Self::MissingParent | Self::CreateTemporary(_) => {
                "the temporary file could not be created; the original remains intact"
            }
            Self::Encode(_) | Self::WriteTemporary(_) | Self::SyncTemporary(_) => {
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
            Self::Read(error) => write!(formatter, "read: {error}"),
            Self::Registry(error) => write!(formatter, "registry: {error}"),
            Self::MpoDisabled => formatter.write_str("MPO processing disabled"),
            Self::GuessFormat(error) => write!(formatter, "format detection: {error}"),
            Self::UnsupportedFormat => formatter.write_str("unsupported format"),
            Self::Dimensions(error) => write!(formatter, "dimensions: {error}"),
            Self::Decode(error) => write!(formatter, "decode: {error}"),
            Self::MissingParent => formatter.write_str("file has no parent directory"),
            Self::CreateTemporary(error) => write!(formatter, "temporary file: {error}"),
            Self::Encode(error) => write!(formatter, "encode: {error}"),
            Self::WriteTemporary(error) => write!(formatter, "temporary write: {error}"),
            Self::SyncTemporary(error) => write!(formatter, "temporary sync: {error}"),
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

    use super::{MAX_IMAGE_DIMENSION, MAX_IMAGE_PIXELS, process_image};
    use crate::domain::{ImageOutcome, JpegQuality, ProcessingPolicy, SkipReason, WorkerCount};
    use crate::ports::{ImageRegistry, ProcessedImage, RegistryError};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn large_camera_photos_are_within_the_limits() {
        // 55.6 MP (reported by a user), 102 MP and 151 MP medium-format sensors.
        for (width, height) in [(8_608_u32, 6_456_u32), (11_648, 8_736), (14_204, 10_652)] {
            assert!(width.max(height) <= MAX_IMAGE_DIMENSION);
            assert!(u64::from(width) * u64::from(height) <= MAX_IMAGE_PIXELS);
        }
    }

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
        // zenjpeg writes a progressive (SOF2) frame.
        let output = std::fs::read(&path)?;
        assert!(output.windows(2).any(|marker| marker == [0xFF, 0xC2]));
        Ok(())
    }

    #[test]
    fn icc_profile_is_kept() -> TestResult {
        let directory = tempdir()?;
        let path = directory.path().join("profiled.jpg");
        let profile: Vec<u8> = (0..=255).collect();
        let image = RgbImage::from_fn(64, 64, |x, y| Rgb([(x * 4) as u8, (y * 4) as u8, 128]));
        {
            // The encoder owns the file; it must be closed before the
            // replacement, which Windows refuses for an open file.
            let mut encoder = JpegEncoder::new_with_quality(File::create(&path)?, 100);
            encoder.set_icc_profile(profile.clone())?;
            encoder.encode_image(&image)?;
        }
        let root = directory.path().canonicalize()?;

        let result = process_image(
            &root,
            &path,
            policy(40)?,
            &mut MemoryRegistry::default(),
            &AtomicBool::new(false),
        );

        assert!(matches!(result.outcome, ImageOutcome::Success { .. }));
        let mut decoder = image::ImageReader::open(&path)?
            .with_guessed_format()?
            .into_decoder()?;
        assert_eq!(
            image::ImageDecoder::icc_profile(&mut decoder)?,
            Some(profile)
        );
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
        // The size is compared in memory, so no temporary file was created.
        assert_eq!(std::fs::read_dir(directory.path())?.count(), 1);
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
