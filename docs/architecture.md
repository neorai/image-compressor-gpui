# Architecture, acceptance, and risks

## Behavioral scope

1. Scan a folder recursively and process `.jpg` and `.jpeg` files without blocking the UI.
2. Replace a file only after a temporary JPEG has been encoded and synchronized successfully.
3. Calculate SHA-256 and skip a content/path pair that is already recorded.
4. Store path, hash, quality, and date in SQLite and detect one hash at multiple paths.
5. Support the primary MPO image and recognizable image content renamed with a JPEG extension.
6. Offer `Thumbs.db` cleanup and the duplicate report that previously lived in a separate script.
7. Keep English as the source/default language and offer runtime Spanish localization.
8. Store all application-owned files in `data/` next to the executable.

## Boundaries and assumptions

- The application uses no network service and stores no secrets. User paths are private data.
- The Python database is not imported silently because its location may differ and automatic import
  could mix histories. A compatible database can be copied manually before first launch.
- Data protection has priority over peak throughput, so parallelism and resource use are bounded.
- The executable directory must be writable so the portable `data` directory can be created.

## Flow

```text
GPUI Kit view (CompressorView entity)
  -> validated RunRequest
  -> coordinator thread
  -> bounded recursive scan without links
  -> bounded task channel
  -> N image workers, one SQLite connection per worker
  -> bounded result channel
  -> batched events on a per-run channel
  -> view task that drains the channel every 50 ms on the UI thread
```

Each run gets its own event channel, so a late event from a finished run can never update a newer
one. The coordinator marks a run as completed before it sends the final event, so a new run
requested in response to that event joins the old thread instead of being rejected. Quitting the
application cancels an active run and waits for the current image before the process exits.

The activity list retains at most 250 rows and the report displays at most 100 duplicate groups.
The duplicate report loads on a background executor and refreshes after every run; a refresh
requested while one is loading is queued rather than dropped.

## Image pipeline

1. Reject links, non-files, read-only files, and files over the size limit.
2. Hash the original and skip it when the same content is already recorded for that path.
3. Decode with dimension and allocation limits, apply the EXIF orientation to the pixels, and keep
   the ICC profile.
4. Encode to a temporary file in the same directory.
5. If the source is a plain JPEG and the result is not smaller, discard the temporary file and
   record the original, so later runs do not degrade it.
6. Otherwise flush, synchronize, persist atomically, hash the result, and record it.

## Threat model

**Assets:** original images, SQLite history, application availability, and path privacy.

**Untrusted inputs:** path text, directory trees, file names and formats, image metadata, the
persisted database, and concurrent filesystem changes.

Main mitigations:

- Resource exhaustion: limits for entries, depth, files, bytes, dimensions, decoder allocation,
  workers, and channel capacity.
- Link or junction escape: links are not followed; canonical paths must remain under the root before
  processing or deleting.
- Partial output loss: secure temporary file on the same volume, `flush`, `sync_all`, atomic persist,
  and transactional SQLite updates.
- UI starvation: disk, decoding, hashing, and SQL run off the UI thread; updates are batched.
- Log injection or leakage: control characters and length are restricted and image paths are relative.
- Future or corrupt database: explicit errors and rejection of unsupported `user_version` values.

A TOCTOU race remains possible if another process replaces a directory with a junction between
validation and open. Complete Windows defense would require validated native handles and targeted
unsafe code, which this implementation deliberately avoids without platform-specific tests.

## Acceptance criteria

- The window can choose a folder and settings, switch between English and Spanish, start, show
  progress, and cancel; closing it during a run leaves no temporary file.
- All statistic cards share one two-column grid and remain aligned across rows.
- A valid JPEG is replaced, recorded, and counted; a second run is skipped.
- A rotated camera JPEG keeps its visible orientation after compression.
- A JPEG that would grow is left byte-for-byte unchanged and skipped on the next run.
- Every interface string has a Spanish catalog entry and the catalog has no unused entries.
- A corrupt JPEG, escaped path, or failure before persist leaves the input intact.
- A database failure after replacement is shown as a partial result.
- `data/images.db` and `data/compression.log` are created next to the executable, never in AppData.
- `cargo fmt --all -- --check`, `cargo check --all-targets`, `cargo test`, strict Clippy, and
  `cargo deny check` pass.

## Trade-offs

- **Bundled SQLite:** a self-contained executable at the cost of longer builds and a larger binary.
- **GPUI Kit:** a GPU-accelerated interface with themed, accessible components, at the cost of a
  larger dependency tree and a GPU driver requirement (DirectX 11 on Windows).
- **No automatic image backup:** avoids duplicating potentially hundreds of GiB; users who need
  rollback must work on a copy.
- **Portable local data:** predictable ownership and easy migration, but installation under a
  read-only directory prevents startup until the executable is moved somewhere writable.
