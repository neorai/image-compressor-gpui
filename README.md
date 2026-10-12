# Image Compressor

A portable Windows desktop application that compresses the JPEG images in a folder and its
subfolders, keeps a history of every processed file, and finds duplicate images. Everything runs
locally: images never leave the computer.

It is written in Rust with a [GPUI Kit](https://github.com/longbridge/gpui-kit) interface and the
[Dracula](https://draculatheme.com) color palette.

> **Important:** compression replaces each original JPEG. The replacement is atomic, but no backup
> is kept. Try it on a copy of your images first.

## Download

Get `image-compressor-<version>-windows-x64.exe` from the
[Releases](https://github.com/neorai/image-compressor-gpui/releases) page and check it against
`SHA256SUMS.txt`. There is nothing to install: put the executable in a **writable** folder and run
it.

Requirements: Windows 10 or 11 (x64) and a GPU driver with DirectX 11. No network connection is
needed.

## Usage

1. Choose a folder with **Browse…** or type its path (local and UNC paths are supported).
2. Set the JPEG quality (1–100) and the number of workers (1–32).
3. Choose the options: skip images already recorded, convert the primary MPO image, and delete
   `Thumbs.db` files when finished (off by default).
4. Press **Compress**. Progress, statistics, and recent activity update live; **Cancel** stops the
   run after the image in progress.

The **Duplicates** tab lists images with identical content at different paths. The interface is
available in English and Spanish and switches instantly from the selector in the header.

## Features

- Recursive scan of `.jpg` and `.jpeg` files, processed in parallel without blocking the interface.
- Re-encoding at the chosen quality. A JPEG that would not get smaller is left unchanged, so
  repeated runs never degrade it.
- EXIF orientation is applied to the pixels, so rotated photos stay upright, and the ICC color
  profile is preserved.
- Primary image of MPO files and PNG/BMP/TIFF/WebP content saved with a JPEG extension.
- SHA-256 hashes before and after compression; already processed files are skipped on later runs.
- Duplicate report grouped by content.
- Optional `Thumbs.db` removal, limited to the selected folder.

## Where data is stored

On startup the application creates a `data` folder **next to the executable**:

```text
image-compressor.exe
data/
  images.db          SQLite history: paths, hashes, quality, and dates
  compression.log    Activity log (rotated to compression.log.1 at 5 MiB)
```

Nothing is written to AppData or the registry. To move the application with its history, move the
executable and its `data` folder together; to upgrade, replace only the executable. If the folder
is read-only, a dialog explains the problem at startup. The About dialog shows the exact path.

The database contains the paths of your images, so treat it as private data. The log uses paths
relative to the selected folder.

## Data safety

- Each image is encoded into a temporary file in the same folder, synchronized to disk, and only
  then atomically replaces the original. If anything fails before that point, the original is
  untouched.
- Read-only files, symbolic links, and junctions are skipped.
- Limits protect against oversized inputs: 250,000 JPEG files and 128 directory levels per run;
  1 GiB, 20,000 pixels per side, and 40 million pixels per image.
- Closing the window during a run cancels it cleanly and leaves no temporary files.

## Known limitations

- EXIF metadata other than orientation (camera, dates, GPS) is not copied to the compressed file.
- Only the primary image of an MPO file is kept.
- Cancellation waits for the image currently being decoded or encoded.

## Building from source

Requires Rust 1.96 (pinned in `rust-toolchain.toml`).

```powershell
cargo build --release
```

The executable is written to `target/release/image-compressor.exe`.

Quality checks, also run by CI on every push:

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check
```

### Project layout

```text
src/domain/          Validated types, states, and statistics
src/app.rs           Run coordination, cancellation, and workers
src/ports/           Registry and logging traits
src/infrastructure/  SQLite, image processing, hashing, scanning, paths, and logging
src/ui/              GPUI Kit view, theme, messages, and localization
translations/        Spanish gettext catalog
docs/                Architecture and risk notes
```

See [docs/architecture.md](docs/architecture.md) for the design, threat model, and trade-offs.

## License

MIT. GPUI Kit and GPUI are used under Apache-2.0; the icons are from [Lucide](https://lucide.dev)
(ISC).
