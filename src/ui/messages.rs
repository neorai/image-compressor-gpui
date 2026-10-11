//! Localized text for the status, activity and report codes emitted by the
//! application layer. Codes keep the background threads free of UI language.

use crate::app::ActivitySource;
use crate::ui::i18n::{Language, tr, trf};

pub fn status(language: Language, code: &str, value: i32, detail: &str) -> String {
    match code {
        "idle" => tr(language, "Select a folder to begin."),
        "preparing" => tr(language, "Preparing the run…"),
        "scanning" => tr(language, "Validating and scanning the folder…"),
        "processing" => trf(language, "Processing {} images…", &[&value]),
        "cancelling" => tr(
            language,
            "Cancelling safely; the current image may take a moment…",
        ),
        "empty-path" => tr(language, "Choose a folder before starting."),
        "path-too-long" => tr(language, "The selected path is too long."),
        "invalid-quality" => tr(language, "JPEG quality must be between 1 and 100."),
        "invalid-workers" => tr(language, "Worker count must be between 1 and 32."),
        "root-unavailable" => tr(language, "The selected folder cannot be accessed."),
        "root-not-directory" => tr(language, "The selected path is not a folder."),
        "root-invalid" => tr(language, "The folder location could not be validated."),
        "scan-failed" => tr(language, "The scan could not be completed."),
        "database-unavailable" => tr(language, "The application database could not be opened."),
        "log-unavailable" => tr(language, "The activity log could not be opened."),
        "background-start-failed" => tr(language, "Background work could not be started."),
        "state-unavailable" => tr(language, "The internal state is unavailable."),
        "already-running" => tr(language, "A compression run is already active."),
        "thread-panicked" => tr(language, "An internal process ended unexpectedly."),
        "completed-incidents" => trf(
            language,
            "Process completed with incidents in {}.",
            &[&detail],
        ),
        "completed" => trf(
            language,
            "Process completed successfully in {}.",
            &[&detail],
        ),
        "cancelled" => trf(language, "Process cancelled safely after {}.", &[&detail]),
        _ => tr(language, "Ready."),
    }
}

pub fn activity(language: Language, code: &str, args: [i32; 3]) -> String {
    let [a, b, c] = args;
    match code {
        "mpo-disabled" => tr(language, "MPO skipped because MPO processing is disabled"),
        "unsupported-format" => tr(
            language,
            "File skipped because its content has an unsupported format",
        ),
        "resource-limit" => tr(language, "File skipped because it exceeds a safety limit"),
        "registry-unavailable" => tr(
            language,
            "The registry could not be queried; the file was not changed",
        ),
        "read-failed" => tr(language, "The file could not be read; it was not changed"),
        "read-only" => tr(language, "The file is read-only; it was not changed"),
        "path-escaped" => tr(
            language,
            "File skipped because it is no longer inside the selected folder",
        ),
        "decode-failed" => tr(
            language,
            "The image could not be decoded; it was not changed",
        ),
        "temporary-create-failed" => tr(
            language,
            "The temporary file could not be created; the original remains intact",
        ),
        "temporary-write-failed" => tr(
            language,
            "The temporary image could not be written; the original remains intact",
        ),
        "replace-failed" => tr(
            language,
            "The file could not be replaced; the original remains intact",
        ),
        "persist-verification-failed" => tr(
            language,
            "The file was replaced, but persistence could not be confirmed",
        ),
        "already-optimized-duplicates" => trf(
            language,
            "Already optimized; {} copies have the same content",
            &[&a],
        ),
        "already-optimized" => tr(language, "Already optimized"),
        "not-smaller" => trf(
            language,
            "Kept the original; re-encoding at {}% would not reduce it ({} → {} KiB)",
            &[&a, &b, &c],
        ),
        "final-hash-failed" => tr(
            language,
            "Optimized, but the final hash could not be calculated",
        ),
        "database-record-failed" => tr(
            language,
            "Optimized, but it could not be recorded in the database",
        ),
        "optimized" => trf(language, "Optimized at {}% ({} → {} KiB)", &[&a, &b, &c]),
        "scan-depth-limit" => tr(language, "A folder exceeding the maximum depth was skipped"),
        "scan-subfolder-unreadable" => tr(language, "A subfolder could not be read"),
        "scan-entry-unreadable" => tr(language, "A directory entry could not be read"),
        "scan-entry-type-unavailable" => tr(language, "An entry type could not be identified"),
        "scan-link-skipped" => tr(language, "A symbolic link or junction was skipped"),
        "worker-database-unavailable" => tr(language, "A worker could not open the registry"),
        "no-workers" => tr(language, "No worker could be started"),
        _ => tr(language, "The file was not processed"),
    }
}

pub fn activity_source(language: Language, source: &ActivitySource) -> String {
    match source {
        ActivitySource::Image(path) => path.clone(),
        ActivitySource::Scan => tr(language, "Scan"),
        ActivitySource::Process => tr(language, "Process"),
        ActivitySource::Database => tr(language, "Database"),
    }
}

#[cfg(test)]
mod tests {
    use super::{activity, status};
    use crate::ui::i18n::Language;

    #[test]
    fn codes_map_to_localized_text() {
        assert_eq!(
            status(Language::Spanish, "processing", 12, ""),
            "Procesando 12 imágenes…"
        );
        assert_eq!(
            activity(Language::English, "optimized", [60, 300, 120]),
            "Optimized at 60% (300 → 120 KiB)"
        );
        assert_eq!(
            activity(Language::Spanish, "unknown-code", [0; 3]),
            "El archivo no se procesó"
        );
    }
}
