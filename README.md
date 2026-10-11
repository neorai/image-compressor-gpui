# Image Compressor (Rust + GPUI Kit)

A local desktop application that recreates and extends the original Python project. It scans a
folder recursively for `.jpg` and `.jpeg` files, re-encodes them with configurable JPEG quality,
tracks content hashes in SQLite, reports duplicates, and can remove `Thumbs.db` files. The
interface is built with [GPUI Kit](https://github.com/longbridge/gpui-kit); disk and CPU work runs
outside the UI thread.

> **Important:** compression replaces each original JPEG. Replacement is atomic, but the
> application does not keep a backup. Test with a copy of your images first. `Thumbs.db` deletion
> is optional and disabled by default.

## Requirements

- Windows 10 or later, or another desktop platform supported by GPUI and Rust.
- Rust 1.96 as pinned in `rust-toolchain.toml`.
- A GPU driver with DirectX 11 (Windows), Metal (macOS), or Vulkan (Linux), as required by GPUI.
- Recommended reference hardware: 4 CPU cores, 4 GiB RAM, and an 860 × 640 display or larger.
- No network connection is required. Images never leave the computer.
- Windows UNC paths are supported when the current user has read and write permission.

## Interface languages

English is the source language and the default on every system. Use the language selector in the
window header to switch to Spanish at runtime. Controls, status messages, validation errors,
activity rows, reports, the window title, and the About dialog all update immediately. Spanish
text comes from the bundled gettext catalog in `translations/`; a test fails if an interface
string has no translation or the catalog keeps an unused entry.

## Features

- Graphical folder picker plus manual entry for local and UNC paths.
- Bounded parallel processing with 1–32 workers.
- JPEG, primary MPO image, and recognizable PNG/BMP/TIFF/WebP content stored with a JPEG extension.
- EXIF orientation is applied to the pixels, so rotated camera photos keep displaying upright, and
  the ICC color profile is preserved.
- A JPEG whose re-encoded version would not be smaller is kept unchanged and recorded, so it is
  not degraded by repeated runs.
- SHA-256 before and after compression.
- Idempotent SQLite records by path and duplicate grouping by content.
- Progress, recent activity (newest first), aligned statistics, cooperative cancellation, and
  duplicate reports that refresh after each run.
- Optional `Thumbs.db` cleanup after confirming every candidate remains inside the selected root.
- A 5 MiB activity log that rotates while the application runs, without full root paths in
  per-image messages.
- A dark interface with the [Dracula](https://draculatheme.com) color palette (`src/ui/dracula.json`).

## Build and run

```powershell
cargo run --release
```

Quality checks:

```powershell
cargo fmt --all -- --check
cargo check --all-targets
cargo test
cargo clippy --all-targets -- -D warnings
cargo deny check
```

Build the distributable binary:

```powershell
cargo build --release
```

The resulting executable is `target/release/image-compressor.exe`. Copy it to any writable
folder; `dist/` is ignored by Git for local builds.

## Portable local data

On startup, the application creates a `data` folder in the executable's directory:

- `data/images.db` stores hashes, paths, JPEG quality, and timestamps.
- `data/compression.log` stores activity; one `.log.1` generation is retained after 5 MiB.

No application file is written to AppData. The executable directory therefore needs write
permission; if it is read-only, the application explains the problem in a dialog instead of
exiting silently. Keep the executable and its `data` folder together to move the application with
its history. The About dialog and duplicate report show the exact effective path.

The database uses `PRAGMA user_version = 1`. Before a future destructive migration, copy the
database, migrate inside a transaction, and restore the copy if migration fails.

## Data safety and limits

- Symbolic links and detected junctions are not followed during scanning.
- The selected root and every image are canonicalized before processing.
- Read-only images are reported and left untouched.
- Each run is limited to 250,000 JPEG files, 1,000,000 entries, and 128 directory levels.
- Each input is limited to 1 GiB, 20,000 pixels per side, 40 million pixels, and 192 MiB of requested
  decoder allocation.
- Output is encoded into a secure temporary file in the same directory, flushed, synchronized, and
  atomically persisted. Only then is the final hash calculated and SQLite updated.
- If SQLite fails after replacement, the result is reported as partial instead of success.
- Cancellation is checked between phases. An active decoder or encoder cannot be interrupted in the
  middle; if replacement has not happened, the temporary file is discarded and the original stays.
  Closing the window during a run cancels it the same way before the process exits.

The database stores image paths and the duplicate view exposes them. Treat the database and report
screenshots as potentially sensitive local data. The activity log uses paths relative to the chosen
folder.

## Project layout

```text
src/domain/          Validated types, states, and statistics
src/app.rs           Coordination, cancellation, bounded channels, workers
src/ports/           Small registry and observability traits
src/infrastructure/  SQLite, image processing, hashing, scanning, paths, logging
src/ui/              GPUI Kit view, localized messages, formatting, and i18n
translations/        Bundled Spanish gettext catalog
docs/                Architecture and risk notes
```

See [docs/architecture.md](docs/architecture.md) for acceptance criteria, security boundaries, and
known trade-offs.

## Known limitations

- MPO processing keeps only the primary decodable image, matching the original workflow.
- Other EXIF metadata (camera, dates, GPS) is not copied to the re-encoded file, matching the
  original workflow; orientation is applied to the pixels instead.
- Cancellation is not instantaneous inside a decoder or encoder call.
- Power-loss resilience depends on the filesystem and storage device, even though temporary and
  final files are synchronized.
- Physical failures such as a full disk, power loss, or a junction swap during processing need a
  controlled test machine or volume and are not automated in CI.

## License

Application code is distributed under MIT. GPUI Kit and GPUI are used under the Apache-2.0
license; the default icons are from [Lucide](https://lucide.dev) (ISC).
