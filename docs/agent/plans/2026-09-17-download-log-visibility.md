# Download Log Visibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to
> implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Make the run log answer "where did this album finally land?" and
"what queue position did this download reach?" — the first as one line per
album naming the real destination, the second as a bounded number of lines
that never grows with queue depth.

**Architecture:** Two independent halves that share only the album completion
line. Part A threads the album folder the library write actually produced
(`LibraryWriteOutcome`) out through `runner` into a new `DownloadDestination`
on `AlbumOutcome::Downloaded`, so the log, the run summary, and the
notification all report it; the staging-only case is modelled as a destination
variant so it can be reported truthfully and cannot be mistaken for a library
path. Part B replaces the immediate `Download queued` log with a deferred
notice that fires at the first of a position observation, a transfer start, or
a 5-second grace, adds one `Download started` line, and demotes
per-position-change updates to `DEBUG` plus an in-place interactive spinner
that carries its own bar counter.

**Tech Stack:** Rust 2021, tokio (and `test-util` for paused-time tests),
`tracing`, `indicatif` 0.17.11, `tempfile` + `wiremock` (dev).

**Spec:** `docs/agent/specs/2026-09-17-download-log-visibility-design.md`

---

<!-- markdownlint-disable MD013 -->

## Scope check

The spec covers two independent subsystems. They touch disjoint regions of the codebase and
either can be delivered, tested, and released without the other:

- **Part A** — destination reporting: `organizer.rs`, `report.rs`, `notifier.rs`, `runner.rs`.
- **Part B** — queue visibility: `progress.rs`, `download.rs`, `formatting.rs`.

They share exactly one file (`src/download.rs`): Part A rewords one log string in
`download_once`'s completion branch, Part B rewrites the queue branch of the same function.
They are kept in one plan document because the chain consumes one plan, but **Part A and
Part B are independently deliverable** — Part A ends after Task A5 with a working, testable
tree, and Part B can be stopped or shipped separately. There is no cross-part dependency, so
the tasks can be executed in the order given or split across two sub-agent lanes.

## File map

| File | Part | Responsibility after this plan |
| --- | --- | --- |
| `src/organizer.rs` | A | Library writes return `LibraryWriteOutcome { album_dir, written }`; `album_dir` is the album folder with any disc subdirectory stripped; each written file logs `Organized: {src} -> {dest}` at `DEBUG` |
| `src/report.rs` | A | `DownloadDestination` (`Library` / `Staging`) with `render()`; `AlbumOutcome::Downloaded` carries it; the summary renders the destination path |
| `src/notifier.rs` | A | `notify_success` takes the rendered destination and appends it to the payload message |
| `src/runner.rs` | A | Four destination paths map onto `DownloadDestination`; `finish_library_write` is the single completion-line emitter and removes staging only for a library destination |
| `README.md` | A, B | Documents the new log lines |
| `src/formatting.rs` | B | `format_duration` for queue-wait reporting |
| `src/progress.rs` | B | `create_queue_bar`, `clear_queue_bar`, `queue_bars_created`, `queue_bars_finished`, `queue_label` |
| `src/download.rs` | A, B | Deferred queue notice, `Download started` line, `DEBUG` position changes, queue-bar lifecycle; `Download staged` replaces `Download completed` |

## Commands used throughout

Run from the repository root, `/data/seakarr`.

```bash
cargo test -p seakarr --lib <filter>   # focused unit tests
cargo test -p seakarr                  # all seakarr tests, lib + tests/
cargo clippy -p seakarr --all-targets  # lint gate
cargo fmt --all                        # formatting
```

`cargo test` at the root also builds the vendored `soulseek-rs-lib` workspace member; `-p
seakarr` scopes a run to this crate.

---

## Part A — Destination reporting

### Task A1: Return the album folder from library writes

**Files:**

- Modify: `src/organizer.rs:293-338` (`organize_file`), `src/organizer.rs:398-439`
  (`copy_to_library`), `src/organizer.rs:442-469` (`place_into_library`),
  `src/organizer.rs:482-570` (`copy_into_library`)
- Modify: `src/runner.rs:658-702` (upgrade call site), `src/runner.rs:720-770` (place call
  site), `src/runner.rs:789-807` (generic organize loop)
- Test: `src/organizer.rs` test module (unit tests live beside the code)

- [ ] **Step 1: Write the failing test for a multi-disc album folder**

Add to the `mod tests` block in `src/organizer.rs`, after
`test_copy_to_library_preserves_multi_disc_subdirectories`:

```rust
    #[test]
    fn copy_to_library_reports_the_album_folder_not_the_disc_folder() {
        // The completion log names the album folder. For a multi-disc album the
        // destination of the first written file is the *disc* folder, so a
        // caller deriving the album folder from `written[0].parent()` would
        // report ".../Test Album/CD 01" instead of ".../Test Album".
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let cd1_dir = staging.path().join("CD 01");
        let cd2_dir = staging.path().join("CD 02");
        fs::create_dir_all(&cd1_dir).unwrap();
        fs::create_dir_all(&cd2_dir).unwrap();
        let cd1_t1 = cd1_dir.join("01 - Track.flac");
        let cd2_t1 = cd2_dir.join("01 - Track.flac");
        fs::write(&cd1_t1, b"cd1 track1").unwrap();
        fs::write(&cd2_t1, b"cd2 track1").unwrap();

        let pattern = "%artist%/%album%/%track% - %title%.%ext%";
        let outcome = copy_to_library(
            &[cd1_t1, cd2_t1],
            library.path(),
            pattern,
            "Test Artist",
            "Test Album",
        )
        .unwrap();

        assert_eq!(
            outcome.album_dir,
            library.path().join("Test Artist/Test Album"),
            "the album folder must exclude the disc subdirectory"
        );
        assert_eq!(outcome.written.len(), 2);
    }
```

- [ ] **Step 2: Write the failing test for an all-kept placement**

A placement where every destination already holds audio writes nothing, and the album folder
must still be reported. Add beside the previous test:

```rust
    #[test]
    fn place_into_library_reports_the_album_folder_when_nothing_is_written() {
        // `written` is empty when every destination already holds audio (a
        // deliberate keep policy), yet the album is still placed. The album
        // folder must therefore be derivable without inspecting `written`.
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let staged = staging.path().join("01 - Track.flac");
        write_minimal_flac(&staged);

        // Pre-seed the destination with a file that parses as audio, so the
        // placement keeps it instead of copying.
        let existing_dir = library.path().join("On Disk Artist/Test Album");
        fs::create_dir_all(&existing_dir).unwrap();
        write_minimal_flac(&existing_dir.join("01 - Track.flac"));

        let pattern = "%artist%/%album%/%track% - %title%.%ext%";
        let outcome = place_into_library(
            &[staged],
            library.path(),
            pattern,
            "On Disk Artist",
            "Test Album",
        )
        .unwrap();

        assert!(
            outcome.written.is_empty(),
            "the readable destination must be kept, not overwritten"
        );
        assert_eq!(
            outcome.album_dir,
            library.path().join("On Disk Artist/Test Album"),
            "the album folder must be reported even when nothing was written"
        );
    }
```

- [ ] **Step 3: Run both tests to verify they fail**

```bash
cargo test -p seakarr --lib organizer::tests::copy_to_library_reports_the_album_folder_not_the_disc_folder
```

Expected: compile error — `no method named 'album_dir' found for enum 'Result<Vec<PathBuf>, ...>'`
(the same failure appears for the second test).

- [ ] **Step 4: Add `LibraryWriteOutcome` and derive `album_dir`**

In `src/organizer.rs`, insert the struct immediately above `organize_file`:

```rust
/// Result of a library write: the album folder targeted and the files written.
///
/// `album_dir` is reported to the operator as the album's final destination, so
/// it must be the album folder a human recognises — never a disc subdirectory,
/// and never a path recomputed from the organize pattern (which the
/// sanitisation pass and the verbatim artist component both rewrite).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryWriteOutcome {
    /// The album folder the write targeted, with any disc subdirectory removed.
    /// Present even when `written` is empty.
    pub album_dir: PathBuf,
    /// Destinations actually written. Excludes a file whose destination was
    /// kept because the library already held a better or parseable copy.
    pub written: Vec<PathBuf>,
}
```

- [ ] **Step 5: Return the outcome from `organize_file`**

In `src/organizer.rs`, change the signature and the return of `organize_file`:

```rust
pub fn organize_file(input: OrganizeInput<'_>) -> Result<LibraryWriteOutcome> {
```

Immediately after `let mut dest = input.library_root.join(&relative);` insert:

```rust
    // The album folder is the destination's parent *before* the disc
    // subdirectory is inserted below, so a multi-disc album reports the album
    // folder rather than the disc folder.
    let album_dir = dest
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| input.library_root.to_path_buf());
```

Replace the final `Ok(final_dest)` with:

```rust
    Ok(LibraryWriteOutcome {
        album_dir,
        written: vec![final_dest],
    })
```

- [ ] **Step 6: Return the outcome from `copy_into_library`**

In `src/organizer.rs`, change the signature and body:

```rust
fn copy_into_library(write: LibraryWrite<'_>) -> Result<LibraryWriteOutcome> {
```

Replace `let mut dests = Vec::with_capacity(downloaded.len());` with:

```rust
    let mut written = Vec::with_capacity(downloaded.len());
    let mut album_dir: Option<PathBuf> = None;
```

Immediately after `let mut dest = library_root.join(&relative);` insert:

```rust
        // Recorded before the disc subdirectory is inserted below, and before
        // the keep/ replace decision, so the album folder is known even when
        // every destination is kept.
        if album_dir.is_none() {
            album_dir = dest.parent().map(Path::to_path_buf);
        }
```

Replace `dests.push(dest);` with:

```rust
        tracing::debug!("Organized: {} -> {}", src.display(), dest.display());
        written.push(dest);
```

Replace the final `Ok(dests)` with:

```rust
    Ok(LibraryWriteOutcome {
        // `downloaded` is never empty for a completed album, so the fallback is
        // defensive only.
        album_dir: album_dir.unwrap_or_else(|| library_root.to_path_buf()),
        written,
    })
```

- [ ] **Step 7: Update the two public entry points**

`copy_to_library` and `place_into_library` already tail-call `copy_into_library`, so only
their signatures change:

```rust
pub fn copy_to_library(
    downloaded: &[PathBuf],
    library_root: &Path,
    pattern: &str,
    artist: &str,
    album: &str,
) -> Result<LibraryWriteOutcome> {
```

```rust
pub fn place_into_library(
    downloaded: &[PathBuf],
    library_root: &Path,
    pattern: &str,
    artist_dir: &str,
    album: &str,
) -> Result<LibraryWriteOutcome> {
```

- [ ] **Step 8: Adapt the three `runner.rs` call sites so the tree compiles**

Upgrade path (`src/runner.rs:658`), replace `Ok(dests) => {` with `Ok(outcome) => {` and
`&dests,` with `&outcome.written,`.

Place path (`src/runner.rs:720`), replace `Ok(dests) => {` with `Ok(outcome) => {` and both
`dests.len()` occurrences inside the partial-write warning with `outcome.written.len()`.

Generic organize loop (`src/runner.rs:789`), replace the `Ok(_) => {}` arm with:

```rust
                Ok(outcome) => {
                    if library_album_dir.is_none() {
                        library_album_dir = Some(outcome.album_dir);
                    }
                }
```

and declare the accumulator immediately above `let mut organize_ok = true;`:

```rust
    // Album folder the generic organize step wrote into. `None` means the step
    // did not run (organisation disabled, or no configured library path), so the
    // album is staying in staging.
    let mut library_album_dir: Option<PathBuf> = None;
```

- [ ] **Step 9: Run the organizer and runner tests**

```bash
cargo test -p seakarr --lib organizer::tests
cargo test -p seakarr --lib runner::tests
```

Expected: PASS for both. The five new assertions pass, and every existing test still passes
because `written` carries exactly what `dests` carried before.

- [ ] **Step 10: Commit**

```bash
git add src/organizer.rs src/runner.rs
git commit -m "feat: report the album folder from library writes"
```

### Task A2: Carry the destination on the download outcome

**Files:**

- Modify: `src/report.rs:1-30` (enum), `src/report.rs:28` (`downloaded` field),
  `src/report.rs:40-56` (`record`), `src/report.rs:82-89` (summary rendering)
- Modify: `src/runner.rs:169-199` (`finish_library_write`), `src/runner.rs:198`
- Test: `src/report.rs` test module, `src/runner.rs` test module

- [ ] **Step 1: Write the failing summary tests**

Add to the `mod tests` block in `src/report.rs`:

```rust
    #[test]
    fn summary_renders_the_library_destination() {
        let mut report = RunReport::new();
        report.record(
            "Aquasky",
            "Shadow Era Pt. 1",
            AlbumOutcome::Downloaded {
                track_count: 8,
                destination: DownloadDestination::Library(PathBuf::from(
                    "/media/Music/Paul/Albums/Aquasky/Shadow Era Pt. 1",
                )),
            },
        );
        assert_eq!(
            report.summary_lines(),
            vec![
                "=== Run summary ===".to_string(),
                "Downloaded (1):".to_string(),
                "  Aquasky — Shadow Era Pt. 1 (8 tracks) -> \
                 /media/Music/Paul/Albums/Aquasky/Shadow Era Pt. 1"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn summary_marks_a_staging_destination_as_kept_in_staging() {
        // A staging path is a real final destination, but it must not read as a
        // library location — that confusion is the defect this work fixes.
        let mut report = RunReport::new();
        report.record(
            "Aquasky",
            "Shadow Era Pt. 2",
            AlbumOutcome::Downloaded {
                track_count: 6,
                destination: DownloadDestination::Staging(PathBuf::from(
                    "/downloads/Aquasky--Shadow Era Pt. 2",
                )),
            },
        );
        assert_eq!(
            report.summary_lines(),
            vec![
                "=== Run summary ===".to_string(),
                "Downloaded (1):".to_string(),
                "  Aquasky — Shadow Era Pt. 2 (6 tracks) -> \
                 /downloads/Aquasky--Shadow Era Pt. 2 (kept in staging)"
                    .to_string(),
            ]
        );
    }
```

Add `use std::path::PathBuf;` to that test module's imports if it is not already in scope.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p seakarr --lib report::tests::summary_renders_the_library_destination
```

Expected: compile error — `cannot find type 'DownloadDestination' in this scope`.

- [ ] **Step 3: Add `DownloadDestination` and `render()`**

In `src/report.rs`, insert above `pub enum AlbumOutcome`:

```rust
/// Where a completed album ended up.
///
/// The two variants are deliberately distinct rather than a path plus a boolean:
/// the completion log, the run summary, and the notification all phrase the
/// destination differently, and a staging path that read as a library location is
/// the confusion this type exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadDestination {
    /// Written into the library at this album folder.
    Library(PathBuf),
    /// Deliberately left in staging; this is the album's final location.
    Staging(PathBuf),
}

impl DownloadDestination {
    /// The path to report, with the staging form marked so it cannot be read as
    /// a library location.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Library(path) => path.display().to_string(),
            Self::Staging(path) => format!("{} (kept in staging)", path.display()),
        }
    }
}
```

- [ ] **Step 4: Add the field to the outcome**

Replace the `Downloaded` variant in `AlbumOutcome`:

```rust
    Downloaded {
        track_count: usize,
        destination: DownloadDestination,
    },
```

- [ ] **Step 5: Carry the destination through `RunReport`**

Replace the `downloaded` field declaration:

```rust
    downloaded: Vec<DownloadedAlbum>, // completion order
```

Add the entry struct above `RunReport`:

```rust
/// One successfully downloaded album and where it landed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DownloadedAlbum {
    artist: String,
    album: String,
    track_count: usize,
    destination: DownloadDestination,
}
```

Replace the `Downloaded` arm of `record`:

```rust
            AlbumOutcome::Downloaded {
                track_count,
                destination,
            } => {
                self.downloaded.push(DownloadedAlbum {
                    artist: artist.to_string(),
                    album: album.to_string(),
                    track_count,
                    destination,
                });
            }
```

Replace the summary rendering of the downloaded section:

```rust
        if !self.downloaded.is_empty() {
            lines.push(format!("Downloaded ({}):", self.downloaded.len()));
            lines.extend(self.downloaded.iter().map(|entry| {
                format!(
                    "  {} — {} ({} tracks) -> {}",
                    entry.artist,
                    entry.album,
                    entry.track_count,
                    entry.destination.render()
                )
            }));
        }
```

- [ ] **Step 6: Update the existing report tests**

`test_record_downloaded`, `test_mixed_outcomes`, `test_ordering_preserved`, and
`no_candidates_renders_in_the_failed_section` in `src/report.rs` construct
`AlbumOutcome::Downloaded { track_count: N }`. Give each a staging destination, which is the
value a caller with no library write would pass:

```rust
                destination: DownloadDestination::Staging(PathBuf::from("/downloads/album")),
```

`test_ordering_preserved` also asserts on the tuple form; replace

```rust
        assert_eq!(
            report.downloaded[0],
            ("Z".to_string(), "first".to_string(), 1)
        );
```

with

```rust
        assert_eq!(report.downloaded[0].artist, "Z");
        assert_eq!(report.downloaded[0].album, "first");
        assert_eq!(report.downloaded[0].track_count, 1);
```

- [ ] **Step 7: Update `finish_library_write` to accept and return the destination**

In `src/runner.rs`, add the parameter and thread it:

```rust
async fn finish_library_write(
    config: &Config,
    db: &Database,
    album_staging: &Path,
    artist: &str,
    album: Option<&str>,
    track_count: usize,
    destination: DownloadDestination,
) -> Result<AlbumOutcome> {
```

Replace the final log and return:

```rust
    tracing::info!(
        "Completed: {artist} - {} ({track_count} tracks) -> {}",
        album.unwrap_or("(all)"),
        destination.render()
    );
    Ok(AlbumOutcome::Downloaded {
        track_count,
        destination,
    })
```

Update the two call sites (upgrade at `runner.rs:658`, place at `runner.rs:720`) to pass the
album folder the write reported. In both, replace

```rust
                    let track_count = downloaded.len();
                    return finish_library_write(
                        config,
                        db,
                        &album_staging,
                        artist,
                        album,
                        track_count,
                    )
                    .await;
```

with

```rust
                    let track_count = downloaded.len();
                    return finish_library_write(
                        config,
                        db,
                        &album_staging,
                        artist,
                        album,
                        track_count,
                        DownloadDestination::Library(outcome.album_dir),
                    )
                    .await;
```

Add the import at the top of `src/runner.rs`:

```rust
use crate::report::{AlbumOutcome, DownloadDestination, RunReport};
```

At `runner.rs:853` (the generic organize path's return, not yet consolidated), update the
tail to compile:

```rust
    let track_count = downloaded.len();
    finish_library_write(
        config,
        db,
        &album_staging,
        artist,
        album,
        track_count,
        DownloadDestination::Staging(album_staging.clone()),
    )
    .await
```

- [ ] **Step 8: Update the nine runner test assertions**

Add these helpers to the `mod tests` block in `src/runner.rs`:

```rust
    /// A completed album whose write landed in the library.
    fn downloaded_to(track_count: usize, album_dir: &str) -> AlbumOutcome {
        AlbumOutcome::Downloaded {
            track_count,
            destination: DownloadDestination::Library(PathBuf::from(album_dir)),
        }
    }
```

Then replace each `assert_eq!(result.unwrap(), AlbumOutcome::Downloaded { track_count: N });`
at `src/runner.rs:2074`, `2137`, `5008`, `5088`, `5224`, `5279`, `5360` and `5589`, and the
expectation at `src/runner.rs:5472`, with the equivalent that asserts only what that test is
about:

```rust
        assert!(
            matches!(
                result.unwrap(),
                AlbumOutcome::Downloaded { track_count: 1, .. }
            ),
            "the album must complete with its downloaded track count"
        );
```

Keep each site's own track count. These tests are about download, filtering and processing
flow — not about destination paths — so they assert the track count and leave destination
correctness to Task A3's dedicated tests. This is a deliberate, recorded reduction: the
assertion still fails if the album does not complete with that track count.

- [ ] **Step 9: Run the tests**

```bash
cargo test -p seakarr --lib report::tests
cargo test -p seakarr --lib runner::tests
```

Expected: PASS. The two new summary tests prove the destination rendering; the runner tests
prove the outcome still carries the right track count.

- [ ] **Step 10: Commit**

```bash
git add src/report.rs src/runner.rs
git commit -m "feat: carry the album destination on the download outcome"
```

### Task A3: Log the final album destination on completion

**Files:**

- Modify: `src/runner.rs:772-856` (generic organize tail and the duplicated completion block)
- Modify: `src/download.rs:606` (staging line)
- Test: `src/runner.rs` test module, `src/download.rs` test module

- [ ] **Step 1: Write the failing completion-line tests**

Add to the `mod tests` block in `src/runner.rs`. The existing tests in this module build a
`Config` with `MockClient` and call `process_album`; copy the setup from
`test_process_album_downloads_and_organizes` (`src/runner.rs:2040`) and change only the
config and the assertion.

```rust
    #[tokio::test]
    async fn completion_line_names_the_library_album_folder() {
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 1000,
            slots: 1,
            files: vec![make_file("01 - Track.flac", 900, 1_000_000)],
        }];
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();
        let library = TempDir::new().unwrap();

        let mut config = Config::default();
        config.storage.organize = true;
        config.storage.organize_pattern = "%artist%/%album%/%track% - %title%.%ext%".into();
        config.library.paths = vec![library.path().to_string_lossy().to_string()];

        let capture = crate::test_support::LogCapture::start();
        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            false,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let expected = library
            .path()
            .join("Test Artist/Test Album")
            .display()
            .to_string();
        assert_eq!(
            result,
            AlbumOutcome::Downloaded {
                track_count: 1,
                destination: DownloadDestination::Library(PathBuf::from(expected.clone())),
            }
        );

        let logs = capture.text();
        let line = logs
            .lines()
            .find(|line| line.contains("Completed: Test Artist"))
            .unwrap_or_else(|| panic!("no completion line, got:\n{logs}"));
        assert!(
            line.contains(&expected),
            "the completion line must name the album folder, got: {line}"
        );
        assert!(
            !line.contains("(kept in staging)"),
            "a library write must not be reported as staging, got: {line}"
        );
    }

    #[tokio::test]
    async fn completion_line_marks_a_staging_only_album() {
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "peer".into(),
            speed: 1000,
            slots: 1,
            files: vec![make_file("01 - Track.flac", 900, 1_000_000)],
        }];
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let mut config = Config::default();
        config.storage.organize = false;
        config.library.paths.clear();

        let capture = crate::test_support::LogCapture::start();
        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            false,
            &config,
            &db,
            staging.path(),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert!(
            matches!(
                result,
                AlbumOutcome::Downloaded {
                    destination: DownloadDestination::Staging(_),
                    ..
                }
            ),
            "an album with no library write must be reported as staging"
        );

        let logs = capture.text();
        let line = logs
            .lines()
            .find(|line| line.contains("Completed: Test Artist"))
            .unwrap_or_else(|| panic!("no completion line, got:\n{logs}"));
        assert!(
            line.contains("(kept in staging)"),
            "a staging destination must be marked, got: {line}"
        );
        // The album is still in staging — reporting it must not have deleted it.
        assert!(
            staging.path().exists(),
            "a staging destination must not remove the staging directory"
        );
    }
```

- [ ] **Step 2: Write the failing staging-line test**

Add to the `mod tests` block in `src/download.rs`:

```rust
    #[tokio::test]
    async fn staging_line_says_staged_not_completed() {
        // "Download completed: ... -> <staging path>" read as the album's final
        // location. The line must say where the file was staged.
        let client = MockClient::new();
        let dir = TempDir::new().unwrap();
        let file = make_file("01 - track.flac", 900, 10_000_000);
        let capture = crate::test_support::LogCapture::start();

        download_file(
            &client,
            &file,
            "peer",
            dir.path(),
            &default_dl_config(),
            &default_filter_config_test(),
            None,
            None,
        )
        .await
        .unwrap();

        let logs = capture.text();
        assert!(
            logs.contains("Download staged: 01 - track.flac"),
            "the staging line must be labelled as staging, got:\n{logs}"
        );
        assert!(
            !logs.contains("Download completed:"),
            "no line may call a staging path completed, got:\n{logs}"
        );
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

```bash
cargo test -p seakarr --lib runner::tests::completion_line_names_the_library_album_folder
```

Expected: FAIL — the log line contains `Completed: Test Artist - Test Album (1 tracks)` with no
path, so `line.contains(&expected)` fails.

```bash
cargo test -p seakarr --lib download::tests::staging_line_says_staged_not_completed
```

Expected: FAIL — the log contains `Download completed:`.

- [ ] **Step 4: Reword the staging line**

In `src/download.rs`, replace the completion log at line 606:

```rust
                tracing::info!("Download staged: {basename} -> {}", dest.display());
```

- [ ] **Step 5: Consolidate the generic organize path onto `finish_library_write`**

In `src/runner.rs`, replace everything from the `// Mark processed` comment through the
generic path's final `Ok(AlbumOutcome::Downloaded { ... })` (that is, the block ending at
`runner.rs:856`) with:

```rust
    // Mark processed — only success if organize also succeeded. Albums without
    // an artist are not recorded because ("", album) is not an unambiguous key.
    // Staging removal and the completion line now live in
    // `finish_library_write`, which removes staging only for a library
    // destination: when organisation did not run, staging *is* the album and
    // deleting it would destroy the download.
    if !organize_ok {
        mark_album_processed_if_identifiable(db, artist, album, "failed")?;
        return Ok(AlbumOutcome::Failed {
            reason: "download succeeded but file organization failed".into(),
        });
    }

    let destination = match library_album_dir {
        Some(album_dir) => DownloadDestination::Library(album_dir),
        None => DownloadDestination::Staging(album_staging.clone()),
    };
    let track_count = downloaded.len();
    finish_library_write(
        config,
        db,
        &album_staging,
        artist,
        album,
        track_count,
        destination,
    )
    .await
```

- [ ] **Step 6: Make staging removal depend on the destination**

In `src/runner.rs`, replace the staging removal inside `finish_library_write`:

```rust
    // A library write has moved the album out of staging, so the staging copy is
    // dropped. A staging destination means staging *is* the album's final
    // location — removing it would delete the album.
    if matches!(destination, DownloadDestination::Library(_)) {
        if let Err(e) = std::fs::remove_dir_all(album_staging) {
            tracing::warn!("Failed to remove staging dir {album_staging:?}: {e}");
        }
    }
```

- [ ] **Step 7: Run the tests**

```bash
cargo test -p seakarr --lib runner::tests
cargo test -p seakarr --lib download::tests
cargo test -p seakarr
```

Expected: PASS. The two completion-line tests pass, the staging-line test passes, and the
full seakarr suite still passes — including the existing staging-removal and
organize-disabled tests, which the destination-driven removal must preserve.

- [ ] **Step 8: Commit**

```bash
git add src/runner.rs src/download.rs
git commit -m "feat: log the final album destination on completion"
```

### Task A4: Include the destination in notifications

**Files:**

- Modify: `src/notifier.rs:24-40` (`notify_success`)
- Modify: `src/runner.rs:181-190` (the `notify_success` call in `finish_library_write`)
- Test: `src/notifier.rs` test module

- [ ] **Step 1: Write the failing payload test**

In `src/notifier.rs`, extend the existing
`test_notify_posts_the_documented_payload_shape` mock body and call so it asserts the
destination:

```rust
    #[tokio::test]
    async fn test_notify_payload_names_the_destination() {
        // The operator asked for the album's final path in the notification too,
        // so an Apprise/ntfy alert says where the album was placed.
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/notify"))
            .and(body_json(serde_json::json!({
                "title": "Seakarr — Download Complete",
                "message": "Downloaded \"Test Artist — Test Album\" (2 tracks) to \
                            /media/Music/Test Artist/Test Album",
                "type": "success",
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock_server)
            .await;

        notify_success(
            &[format!("{}/notify", mock_server.uri())],
            "Test Artist",
            "Test Album",
            2,
            "/media/Music/Test Artist/Test Album",
        )
        .await
        .unwrap();
    }
```

`expect(1)` makes the mock fail the test if the body does not match, so this asserts the
message shape without duplicating the formatter.

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -p seakarr --lib notifier::tests::test_notify_payload_names_the_destination
```

Expected: compile error — `this function takes 4 arguments but 5 arguments were supplied`.

- [ ] **Step 3: Add the destination parameter**

In `src/notifier.rs`:

```rust
pub async fn notify_success(
    urls: &[String],
    artist: &str,
    album: &str,
    track_count: usize,
    destination: &str,
) -> Result<()> {
```

and the message:

```rust
        message: format!("Downloaded \"{artist} — {album}\" ({track_count} tracks) to {destination}"),
```

Update the doc comment above the function to record that `destination` is the already-rendered
path, including the `(kept in staging)` marker for a staging destination.

- [ ] **Step 4: Update every call site and the remaining notifier tests**

`src/runner.rs:181`: add the destination argument to the `notify_success` call inside
`finish_library_write`:

```rust
        destination.render().as_str(),
```

The remaining tests in `src/notifier.rs` (`test_notify_sends_payload`,
`test_notify_posts_the_documented_payload_shape`,
`test_notify_accepts_an_undeliverable_scheme_without_failing_the_run`,
`test_notify_empty_urls_is_noop`, `test_notify_multiple_urls`) must gain a destination
argument; pass `"/media/Music/Test Artist/Test Album"` in each, and update
`test_notify_posts_the_documented_payload_shape`'s expected message to end with the
suffix `to /media/Music/Test Artist/Test Album`.

- [ ] **Step 5: Run the tests**

```bash
cargo test -p seakarr --lib notifier::tests
cargo test -p seakarr
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/notifier.rs src/runner.rs
git commit -m "feat: include the album destination in notifications"
```

### Task A5: Document the destination log lines

**Files:**

- Modify: `README.md` (insert after the `### logging` table, before `### pid`)

- [ ] **Step 1: Insert the documentation**

Insert immediately before the `### pid` heading in `README.md`:

````markdown
#### Download log lines

Every completed album produces one completion line naming its final destination:

```text
INFO seakarr::runner: Completed: Aquasky - Shadow Era Pt. 1 (8 tracks) -> /media/Music/Paul/Albums/Aquasky/Shadow Era Pt. 1
```

The path is the album folder the write actually produced, after name sanitisation and disc
subdirectory handling — not the raw `storage.organize_pattern`. When `storage.organize` is
`false`, or `library.paths` is empty, the album stays where it was downloaded and the line
says so:

```text
INFO seakarr::runner: Completed: Aquasky - Shadow Era Pt. 2 (6 tracks) -> /downloads/Aquasky--Shadow Era Pt. 2 (kept in staging)
```

The per-file line that appears as each track finishes reports the **staging** location, and is
labelled accordingly so it cannot be mistaken for the final destination:

```text
INFO seakarr::download: Download staged: 08 Moondance.flac -> /downloads/Aquasky--Shadow Era Pt. 1/08 Moondance.flac
```

The same destination appears in the end-of-run summary and in the `message` of each success
notification. Per-file library destinations are available at `DEBUG`:

```text
DEBUG seakarr::organizer: Organized: /downloads/Aquasky--Shadow Era Pt. 1/08 Moondance.flac -> /media/Music/Paul/Albums/Aquasky/Shadow Era Pt. 1/08 - Moondance.flac
```
````

- [ ] **Step 2: Check the documentation renders and lints**

```bash
markdownlint README.md
```

Expected: no new errors. `README.md` already disables `MD013` (line length) on its first
line, so the long log lines are permitted.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: document the download destination log lines"
```

**Part A is complete and independently shippable at this point.**

---

## Part B — Queue visibility

### Task B1: Add a queue position progress bar and a duration formatter

**Files:**

- Modify: `src/formatting.rs:31-45` (add `format_duration` beside `format_speed`)
- Modify: `src/progress.rs:15-45` (`ProgressDisplay` fields and constructors),
  `src/progress.rs:44-75` (`create_bar`, extracted label sanitising)
- Test: `src/formatting.rs` test module, `src/progress.rs` test module

- [ ] **Step 1: Write the failing duration formatter tests**

Add to the `mod tests` block in `src/formatting.rs`:

```rust
    #[test]
    fn format_duration_seconds_only() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_duration_minutes_and_seconds() {
        // Matches the reported example: a 21m 40s queue wait.
        assert_eq!(format_duration(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_duration(Duration::from_secs(1_300)), "21m 40s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(Duration::from_secs(3_600)), "1h 0m");
        assert_eq!(format_duration(Duration::from_secs(3_780)), "1h 3m");
    }
```

Add `use std::time::Duration;` to that test module.

- [ ] **Step 2: Write the failing queue-bar tests**

Add to `src/progress.rs` (create a `#[cfg(test)] mod tests` block at the end of the file):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_bar_counter_is_separate_from_the_transfer_bar_counter() {
        // The transfer-bar counter is the evidence for the documented
        // "no bar before the transfer starts" contract. A queue bar exists
        // before any transfer, so it must not move that counter.
        let display = ProgressDisplay::new();
        let bar = display.create_queue_bar("01 - Track.flac - peer queue #3");
        assert_eq!(display.queue_bars_created(), 1);
        assert_eq!(
            display.created_bars(),
            0,
            "creating a queue bar must not count as creating a transfer bar"
        );
        display.clear_queue_bar(bar);
        assert_eq!(display.queue_bars_finished(), 1);
    }

    #[test]
    fn transfer_bar_counter_is_separate_from_the_queue_bar_counter() {
        let display = ProgressDisplay::new();
        let bar = display.create_bar("01 - Track.flac", 1_000);
        assert_eq!(display.created_bars(), 1);
        assert_eq!(display.queue_bars_created(), 0);
        bar.finish_and_clear();
    }

    #[test]
    fn queue_label_strips_control_characters_and_names_the_position() {
        // The filename and the peer name are both peer-supplied; a raw escape
        // sequence would be interpreted by the terminal.
        let label = queue_label("01 - Track\u{1b}[31m.flac", "peer\u{7}", Some(42));
        assert!(
            !label.chars().any(char::is_control),
            "control characters must be stripped, got {label:?}"
        );
        assert!(
            label.contains("queue #42"),
            "the position must appear, got {label:?}"
        );
    }

    #[test]
    fn queue_label_reports_an_unknown_position() {
        let label = queue_label("01 - Track.flac", "peer", None);
        assert_eq!(label, "01 - Track.flac - peer queue position unknown");
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

```bash
cargo test -p seakarr --lib formatting::tests::format_duration_minutes_and_seconds
cargo test -p seakarr --lib progress::tests::queue_bar_counter_is_separate_from_the_transfer_bar_counter
```

Expected: compile errors — `cannot find function 'format_duration'` and
`no method named 'create_queue_bar'`.

- [ ] **Step 4: Add `format_duration`**

In `src/formatting.rs`, add after `format_speed`:

```rust
/// Format an elapsed duration for queue-wait reporting.
///
/// | Range     | Format      | Example   |
/// |-----------|-------------|-----------|
/// | < 1 min   | `{s}s`      | `45s`     |
/// | < 1 hour  | `{m}m {s}s` | `21m 40s` |
/// | >= 1 hour | `{h}h {m}m` | `1h 3m`   |
pub fn format_duration(elapsed: std::time::Duration) -> String {
    let total = elapsed.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}
```

- [ ] **Step 5: Add the queue-bar API and extract the label sanitiser**

In `src/progress.rs`, add the field and counter:

```rust
pub struct ProgressDisplay {
    multi: MultiProgress,
    /// Monotonic count of progress bars created, so tests (and future
    /// diagnostics) can observe that bars are only created once a transfer
    /// has actually started. Never decremented.
    bars_created: AtomicUsize,
    /// Monotonic count of queue bars created. Tracked separately because a
    /// queue bar deliberately exists before any transfer starts, so folding it
    /// into `bars_created` would destroy the meaning of that contract.
    queue_bars_created: AtomicUsize,
    /// Monotonic count of queue bars released. Must equal
    /// `queue_bars_created` once an attempt finishes: no path may leave a bar
    /// on the terminal.
    queue_bars_finished: AtomicUsize,
}
```

Initialise both to `AtomicUsize::new(0)` in `new()`.

Add the methods:

```rust
    /// Number of queue bars created so far (monotonic).
    pub fn queue_bars_created(&self) -> usize {
        self.queue_bars_created.load(Ordering::SeqCst)
    }

    /// Number of queue bars released so far (monotonic).
    pub fn queue_bars_finished(&self) -> usize {
        self.queue_bars_finished.load(Ordering::SeqCst)
    }

    /// Create a bar for a file waiting in a peer's queue.
    ///
    /// Unlike [`Self::create_bar`] this deliberately exists before any transfer
    /// starts: a queue position is real information while the file waits, and
    /// the bar updates in place, so a deep queue costs one terminal line rather
    /// than one line per position change.
    pub fn create_queue_bar(&self, label: &str) -> ProgressBar {
        self.queue_bars_created.fetch_add(1, Ordering::SeqCst);
        let bar = self.multi.add(ProgressBar::new_spinner());
        let style = ProgressStyle::with_template("  {spinner} {msg}")
            .expect("valid queue bar template")
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");
        bar.set_style(style);
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        bar.set_message(safe_label(label, 80));
        bar
    }

    /// Release a queue bar. Going through the display keeps the "every attempt
    /// releases its queue bar" contract observable, the same way
    /// `bars_created` makes "no bar before the transfer starts" observable.
    pub fn clear_queue_bar(&self, bar: ProgressBar) {
        self.queue_bars_finished.fetch_add(1, Ordering::SeqCst);
        bar.finish_and_clear();
    }
```

Add the shared sanitiser and the label builder at module scope:

```rust
/// Strip control characters from peer-supplied text and truncate it. Shared by
/// the transfer bar and the queue bar: both render text a peer chose, and a raw
/// control sequence would be interpreted by the terminal.
fn safe_label(text: &str, max_chars: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect()
}

/// Message for a queued file's bar: `08 Moondance.flac - peer queue #42`.
///
/// Both the filename and the peer name are peer-supplied, so both are
/// sanitised; the position is a number from the wire.
#[must_use]
pub fn queue_label(basename: &str, username: &str, position: Option<u32>) -> String {
    let base = safe_label(basename, 48);
    let peer = safe_label(username, 24);
    match position {
        Some(position) => format!("{base} - {peer} queue #{position}"),
        None => format!("{base} - {peer} queue position unknown"),
    }
}
```

Replace the label handling inside `create_bar` so both bars share the sanitiser:

```rust
        // Show only the basename, strip control characters (terminal injection
        // defence against peer-supplied filenames), and truncate to 60 chars.
        let display_name = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
        bar.set_message(safe_label(display_name, 60));
```

Add `use indicatif::ProgressStyle;` to the imports at the top of `src/progress.rs`.

- [ ] **Step 6: Run the tests**

```bash
cargo test -p seakarr --lib formatting::tests
cargo test -p seakarr --lib progress::tests
```

Expected: PASS. The existing
`download::tests::progress_bar_not_created_until_transfer_starts` must also still pass:

```bash
cargo test -p seakarr --lib download::tests::progress_bar_not_created_until_transfer_starts
```

- [ ] **Step 7: Commit**

```bash
git add src/formatting.rs src/progress.rs
git commit -m "feat: add a queue position progress bar"
```

### Task B2: Log the queue position and the download start on one line each

**Files:**

- Modify: `src/download.rs:425-435` (remove the immediate log, add the deferred notice state),
  `src/download.rs:502-510` (poll window), `src/download.rs:640-705` (status arms)
- Test: `src/download.rs` test module

- [ ] **Step 1: Write the failing queue-notice tests**

Add to the `mod tests` block in `src/download.rs`, beside the other queue-state tests:

```rust
    #[tokio::test(start_paused = true)]
    async fn queued_line_carries_the_first_observed_position() {
        // The position is not known when the download is enqueued, so the notice
        // is deferred to the first observation and the operator gets one line
        // with the position rather than one line with and one line without.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(42)),
            status_step(Duration::from_secs(60), in_progress(10_000_000)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let config = default_dl_config();
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_script(&client, 0, 10_000_000, &config, None).await;
        assert!(result.is_ok(), "expected a completed transfer, got {result:?}");

        let logs = capture.text();
        assert_eq!(
            logs.matches("Download queued: 01.flac").count(),
            1,
            "exactly one queued line, got:\n{logs}"
        );
        assert!(
            logs.contains("Download queued: 01.flac from peer - position 42"),
            "the queued line must carry the position, got:\n{logs}"
        );
        assert!(
            logs.contains("Download started: 01.flac from peer after 1m 1s queued (last position 42)"),
            "the started line must carry the wait and last position, got:\n{logs}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn free_slot_attempt_reports_an_immediate_start() {
        // A candidate admitted on an advertised free slot starts without a queue
        // position. It must still produce both lines, and the started line must
        // be distinguishable from a queued wait.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::ZERO, in_progress(10_000_000)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let config = default_dl_config();
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_script(&client, 1, 10_000_000, &config, None).await;
        assert!(result.is_ok(), "expected a completed transfer, got {result:?}");

        let logs = capture.text();
        assert!(
            logs.contains("Download queued: 01.flac from peer\n")
                || logs.contains("Download queued: 01.flac\r\n")
                || logs.contains("Download queued: 01.flac "),
            "the queued line must appear without a position, got:\n{logs}"
        );
        assert!(
            !logs.contains("position 0"),
            "an unknown position must not be reported as position 0, got:\n{logs}"
        );
        assert!(
            logs.contains("Download started: 01.flac from peer immediately (free slot)"),
            "a free-slot start must say so, got:\n{logs}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn silent_peer_still_gets_a_queued_line_at_the_grace_deadline() {
        // The whole point of the report: a peer that never answers must not
        // leave the run silent while the file waits.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(3_600), in_progress(10_000_000)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let config = default_dl_config();
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_script(&client, 0, 10_000_000, &config, None).await;
        assert!(result.is_ok(), "expected a completed transfer, got {result:?}");

        let logs = capture.text();
        assert_eq!(
            logs.matches("Download queued: 01.flac from peer").count(),
            1,
            "the queued line must be emitted once, at the grace deadline, got:\n{logs}"
        );
        assert!(
            !logs.contains(" - position "),
            "no position was reported, so none may be printed, got:\n{logs}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn position_changes_after_the_notice_are_debug_only() {
        // A queue hundreds deep must not produce one INFO line per step: the
        // depth problem is a volume problem, so mid-queue updates are demoted.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(42)),
            status_step(Duration::from_secs(60), queue_position(9)),
            status_step(Duration::from_secs(60), queue_position(1)),
            status_step(Duration::from_secs(60), in_progress(10_000_000)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let config = default_dl_config();
        let capture = crate::test_support::LogCapture::start();

        let (_dir, result) = download_with_script(&client, 0, 10_000_000, &config, None).await;
        assert!(result.is_ok(), "expected a completed transfer, got {result:?}");

        let logs = capture.text();
        assert_eq!(
            logs.matches("Download queued: 01.flac").count(),
            1,
            "only the first observation may reach INFO, got:\n{logs}"
        );
        assert!(
            logs.contains("Download started: 01.flac from peer after 3m 1s queued (last position 1)"),
            "the started line must report the last position seen, got:\n{logs}"
        );
        let debug_lines = logs
            .lines()
            .filter(|line| line.contains("Queue position for 01.flac"))
            .count();
        assert!(
            debug_lines >= 2,
            "every position change must be visible at DEBUG, got:\n{logs}"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p seakarr --lib download::tests::queued_line_carries_the_first_observed_position
```

Expected: FAIL — the log contains `Download queued: 01.flac from peer` with no position, and no
`Download started` line.

- [ ] **Step 3: Add the grace constant and the two line emitters**

In `src/download.rs`, beside `STATUS_POLL_INTERVAL`:

```rust
/// How long the queued notice waits for a queue position before it is emitted
/// without one.
///
/// The vendored client requests position telemetry immediately on enqueue and
/// the poll window is `STATUS_POLL_INTERVAL`, so a position normally arrives
/// within a few hundred milliseconds. The grace exists only so a peer that never
/// answers cannot leave the run silent — the defect this notice fixes.
const QUEUE_NOTICE_GRACE: Duration = Duration::from_secs(5);
```

Add the emitters beside the other queue helpers (below `reject_queued_attempt`):

```rust
/// Announce that a file has been queued.
///
/// Deferred rather than logged at enqueue time: the position is not known until
/// the peer answers, and one line carrying it is what the operator asked for. A
/// `None` position means the notice fired on the transfer-start or grace path.
fn emit_queue_notice(basename: &str, username: &str, position: Option<u32>) {
    match position {
        Some(position) => {
            tracing::info!("Download queued: {basename} from {username} - position {position}");
        }
        None => tracing::info!("Download queued: {basename} from {username}"),
    }
}

/// Announce that a queued file has started transferring, with the wait it
/// served. The free-slot form is used when the transfer began inside the notice
/// grace and no position was ever reported, so it never queued in any
/// meaningful sense.
fn emit_download_started(
    basename: &str,
    username: &str,
    wait: Duration,
    last_position: Option<u32>,
) {
    if last_position.is_none() && wait < QUEUE_NOTICE_GRACE {
        tracing::info!("Download started: {basename} from {username} immediately (free slot)");
        return;
    }
    match last_position {
        Some(position) => tracing::info!(
            "Download started: {basename} from {username} after {} queued (last position {position})",
            crate::formatting::format_duration(wait)
        ),
        None => tracing::info!(
            "Download started: {basename} from {username} after {} queued",
            crate::formatting::format_duration(wait)
        ),
    }
}
```

- [ ] **Step 4: Replace the immediate log with the deferred notice state**

In `download_once`, delete `tracing::info!("Download queued: {basename} from {username}");`
(line 431) and add to the state block below it:

```rust
    // The queued notice is deferred until one of three things happens: the peer
    // reports a position, the transfer starts, or the grace expires. `emit_once`
    // guards all three so the line can never be duplicated.
    let notice_grace_deadline = enqueued_at + QUEUE_NOTICE_GRACE;
    let mut notice_emitted = false;
```

- [ ] **Step 5: Emit the notice and the started line on transfer start**

In the `DownloadStatus::InProgress` arm, inside the existing `if transfer_start.is_none()`
block, before `transfer_start = Some(now);`:

```rust
                    if !notice_emitted {
                        emit_queue_notice(basename, username, None);
                        notice_emitted = true;
                    }
                    emit_download_started(
                        basename,
                        username,
                        now.duration_since(enqueued_at),
                        observed_queue_position,
                    );
```

- [ ] **Step 6: Emit the notice on the first position observation**

In the `DownloadStatus::Queued { queue_position }` arm, inside
`if let Some(position) = queue_position.filter(|position| *position > 0) {`, after
`observed_queue_position = Some(position);`:

```rust
                        if !notice_emitted {
                            emit_queue_notice(basename, username, Some(position));
                            notice_emitted = true;
                        } else if observed_queue_position != Some(position) {
                            tracing::debug!(
                                "Queue position for {basename} from {username}: {position}"
                            );
                        }
```

Order the assignment so the comparison sees the previous value: move
`observed_queue_position = Some(position);` to *after* this block. The `DEBUG` line is what
makes every change visible on demand without an `INFO` line per step.

- [ ] **Step 7: Emit the notice at the grace deadline**

In the `Err(_elapsed)` arm (the poll window expiring with no status), before the existing
`if transfer_start.is_some()` check:

```rust
                if !notice_emitted && now >= notice_grace_deadline {
                    emit_queue_notice(basename, username, None);
                    notice_emitted = true;
                }
```

- [ ] **Step 8: Run the tests**

```bash
cargo test -p seakarr --lib download::tests
```

Expected: PASS — the four new tests plus every existing queue-state test. The existing
`queued_wait_does_not_consume_transfer_timeout` and the queue-limit tests must be unaffected:
they assert on outcomes and warning text, which this task does not change.

- [ ] **Step 9: Commit**

```bash
git add src/download.rs
git commit -m "feat: log queue position and download start on one line each"
```

### Task B3: Release the queue bar on every queued attempt exit

**Files:**

- Modify: `src/download.rs:424-535` (queue-bar state and creation),
  `src/download.rs:520-535` (fail-closed start), `src/download.rs:585-600` (fail-closed
  complete), `src/download.rs:640-660` (position rejection), `src/download.rs:660-680`
  (channel close), `src/download.rs:680-700` (deadline expiry), `src/download.rs:462-471`
  (cancellation)
- Test: `src/download.rs` test module

- [ ] **Step 1: Write the failing lifecycle tests**

Add to the `mod tests` block in `src/download.rs`. These need a progress-aware variant of
`download_with_script`; add it beside that helper:

```rust
    /// Run one scripted attempt with a progress display, so queue-bar lifecycle
    /// can be observed. Mirrors `download_with_script`.
    async fn download_with_script_and_progress(
        client: &ScriptedClient,
        peer_slots: u8,
        display: &ProgressDisplay,
        config: &DownloadConfig,
    ) -> (TempDir, Result<(PathBuf, f64)>) {
        let dir = TempDir::new().unwrap();
        let file = make_file("Music\\Artist\\Album\\01.flac", 900, 10_000_000);
        let result = download_file_for_candidate(
            client,
            &file,
            "peer",
            peer_slots,
            dir.path(),
            config,
            &default_filter_config_test(),
            Some(display),
            None,
        )
        .await;
        (dir, result)
    }
```

Then the tests:

```rust
    #[tokio::test(start_paused = true)]
    async fn queue_bar_is_replaced_by_the_transfer_bar_and_released() {
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(7)),
            status_step(Duration::from_secs(60), in_progress(10_000_000)),
            status_step(Duration::from_secs(1), DownloadStatus::Completed),
        ]]);
        let display = ProgressDisplay::new();
        let config = default_dl_config();

        let (_dir, result) =
            download_with_script_and_progress(&client, 0, &display, &config).await;
        assert!(result.is_ok(), "expected a completed transfer, got {result:?}");

        assert_eq!(
            display.queue_bars_created(),
            1,
            "a queue bar must exist once a position is observed"
        );
        assert_eq!(
            display.queue_bars_finished(),
            1,
            "the queue bar must be released when the transfer takes over"
        );
        assert_eq!(
            display.created_bars(),
            1,
            "the transfer bar must still be created at transfer start"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn queue_bar_is_released_when_a_position_is_rejected() {
        // An out-of-bound position abandons the attempt. The bar must not be
        // left on the terminal by a path that never starts transferring.
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(500)),
        ]]);
        let display = ProgressDisplay::new();
        let mut config = default_dl_config();
        config.max_queue_length = 5;

        let (_dir, result) =
            download_with_script_and_progress(&client, 0, &display, &config).await;
        assert!(result.is_err(), "an out-of-bound position must reject the attempt");

        assert_eq!(display.queue_bars_created(), 1);
        assert_eq!(
            display.queue_bars_finished(),
            1,
            "rejection must release the queue bar"
        );
        assert_eq!(
            display.created_bars(),
            0,
            "a rejected attempt must never create a transfer bar"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn queue_bar_is_released_when_the_queue_deadline_expires() {
        let client = ScriptedClient::new(vec![vec![
            status_step(Duration::from_secs(1), queue_position(9)),
        ]]);
        let display = ProgressDisplay::new();
        let mut config = default_dl_config();
        config.max_queue_length = 50;
        config.max_queue_time_secs = 60;

        let (_dir, result) =
            download_with_script_and_progress(&client, 0, &display, &config).await;
        assert!(result.is_err(), "expected a queue timeout, got {result:?}");

        assert_eq!(
            display.queue_bars_created(),
            display.queue_bars_finished(),
            "every queue bar must be released on the timeout path"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p seakarr --lib download::tests::queue_bar_is_replaced_by_the_transfer_bar_and_released
```

Expected: FAIL — `left: 0, right: 1` for `queue_bars_created()`, because nothing creates a
queue bar yet.

- [ ] **Step 3: Add the queue-bar state and the release helper**

In `src/download.rs`, add beside the transfer-bar state in `download_once`:

```rust
    // Queue bar for this attempt. Created on the first position observation and
    // released on every path out of the queue, so no attempt can leave a bar
    // behind.
    let mut queue_bar: Option<ProgressBar> = None;
```

Add the module-scope helper beside the other bar helpers:

```rust
/// Release a queue bar if one exists, counting the release through the display so
/// the "every attempt releases its bar" contract stays observable.
///
/// Every path that leaves the queue before the transfer starts must call this:
/// transfer start, position rejection, both fail-closed rejections, queue
/// deadline expiry, channel close, and cancellation.
fn clear_queue_bar(progress: Option<&ProgressDisplay>, queue_bar: &mut Option<ProgressBar>) {
    if let (Some(display), Some(bar)) = (progress, queue_bar.take()) {
        display.clear_queue_bar(bar);
    }
}
```

- [ ] **Step 4: Create and update the bar on a position observation**

In the `DownloadStatus::Queued { queue_position }` arm, inside the validated-position block
added in Task B2, after the notice block:

```rust
                        if let Some(display) = progress {
                            let label = crate::progress::queue_label(
                                basename,
                                username,
                                Some(position),
                            );
                            match &queue_bar {
                                Some(existing) => existing.set_message(label),
                                None => queue_bar = Some(display.create_queue_bar(&label)),
                            }
                        }
```

- [ ] **Step 5: Release the bar when the transfer takes over**

In the `DownloadStatus::InProgress` arm, at the top of the existing
`if transfer_start.is_none()` block (before the notice so the bar is gone before the transfer
bar appears):

```rust
                    clear_queue_bar(progress, &mut queue_bar);
```

- [ ] **Step 6: Release the bar on every abandoning path**

Add `clear_queue_bar(progress, &mut queue_bar);` immediately before each of these returns in
`download_once`:

1. the cancellation return at the top of the loop (`src/download.rs:462-471`);
2. the fail-closed `reject_queued_attempt` return in the `InProgress` arm
   (`src/download.rs:529`);
3. the fail-closed `reject_queued_attempt` return in the `Completed` arm
   (`src/download.rs:594`);
4. the position-rejection `reject_queued_attempt` return in the `Queued` arm
   (`src/download.rs:646`);
5. the channel-close return in the `Ok(None)` arm (`src/download.rs:664`);
6. the `expire_queue_wait` return in the `Err(_elapsed)` arm (`src/download.rs:692`).

Each site already has `progress` and `queue_bar` in scope. Add the call on the line before the
`return`, inside the same block, so it also runs when the queued attempt is abandoned by
cancellation that arrived while queued.

- [ ] **Step 7: Run the tests**

```bash
cargo test -p seakarr --lib download::tests
cargo test -p seakarr
```

Expected: PASS. Every queue test asserts
`queue_bars_created() == queue_bars_finished()` on its exit path, and the existing
`progress_bar_not_created_until_transfer_starts` test still passes because queue bars use their
own counter.

- [ ] **Step 8: Commit**

```bash
git add src/download.rs
git commit -m "feat: release the queue bar on every queued attempt exit"
```

### Task B4: Document the queue log lines

**Files:**

- Modify: `README.md` (append to the `#### Download log lines` subsection added in Task A5)

- [ ] **Step 1: Append the queue documentation**

Append to the `#### Download log lines` subsection in `README.md`:

````markdown
A download that waits in a peer's upload queue reports its position in the same line that
announces it, and reports how long it waited when it finally starts:

```text
INFO seakarr::download: Download queued: 08 Moondance.flac from nottucks - position 42
INFO seakarr::download: Download started: 08 Moondance.flac from nottucks after 21m 40s queued (last position 3)
```

The queue line is emitted once the peer reports a position — normally within a fraction of a
second — or, at the latest, five seconds after the request. A peer that never answers still
produces the line, without a position. A candidate admitted on an advertised free slot that
starts transferring at once reads:

```text
INFO seakarr::download: Download queued: 08 Moondance.flac from nottucks
INFO seakarr::download: Download started: 08 Moondance.flac from nottucks immediately (free slot)
```

Positions are deliberately **not** logged as a live counter: a queue 100 deep would produce
100 lines. Every position change is instead available at `DEBUG`, and in an interactive
terminal a queue bar shows the current position in place, so a long wait costs one terminal
line:

```text
DEBUG seakarr::download: Queue position for 08 Moondance.flac from nottucks: 9
```

Queue timeouts and rejections keep reporting the position in their existing warnings.
````

- [ ] **Step 2: Check the documentation renders and lints**

```bash
markdownlint README.md
```

Expected: no new errors.

- [ ] **Step 3: Verify the full acceptance criteria by hand**

Run the suite and the lint gate:

```bash
cargo test -p seakarr
cargo clippy -p seakarr --all-targets -- -D warnings
```

Expected: all tests pass, no clippy warnings.

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs: document the queue log lines"
```

**Part B is complete and independently shippable at this point.**

---

## Self-review

**Spec coverage.** Every spec section maps to a task:

| Spec requirement | Task |
| --- | --- |
| `LibraryWriteOutcome` with `album_dir` excluding the disc subdirectory | A1 |
| `album_dir` returned when nothing was written | A1 |
| `DownloadDestination` on `AlbumOutcome::Downloaded` | A2 |
| Summary renders the destination, staging marked | A2 |
| Completion line with the destination on all four paths | A3 |
| Staging line relabelled `Download staged` | A3 |
| Staging-only album reported as the destination and not deleted | A3 |
| Notification message carries the destination | A4 |
| `DEBUG` per-file `Organized: {src} -> {dest}` | A1 |
| README notes for the destination lines | A5 |
| Deferred queued notice (position / transfer start / 5 s grace) | B2 |
| `Download started` line, free-slot and last-position forms | B2 |
| Position changes `DEBUG`-only plus spinner | B2 (log), B3 (spinner) |
| Queue bar with a separate counter, created on first position | B1, B3 |
| Queue bar released on every exit | B3 |
| `format_duration` | B1 |
| README notes for the queue lines | B4 |
| Queue policy unchanged (regression) | B2 Step 8, B3 Step 7 |

**Placeholder scan.** No `TBD`/`TODO`/"handle edge cases"/"similar to Task N". Every code
step carries the code; every test step carries the test; every run step carries the command
and its expected result.

**Type consistency.** `LibraryWriteOutcome { album_dir, written }` is introduced in A1 and used
with those exact names in A2/A3. `DownloadDestination::{Library, Staging}` and `render()` are
defined in A2 and used unchanged in A3/A4. `finish_library_write`'s parameter order is
`(config, db, album_staging, artist, album, track_count, destination)` at its definition and
at all three call sites. `keep_queue_bar`-style helpers are consistently named
`clear_queue_bar(progress, queue_bar)` in both its definition and its six call sites.
`notify_success`'s new parameter is last in the signature and last at every call site.
`queue_label(basename, username, position)` matches its four test calls and its one
`download.rs` call site.

**Two recorded deviations from the spec, both deliberate and flagged for review:**

1. The spec's risk note estimated "roughly twenty" test assertions needing the new
   `AlbumOutcome::Downloaded` field. The actual count is nine (`src/runner.rs:2074`, `2137`,
   `5008`, `5088`, `5224`, `5279`, `5360`, `5472`, `5589`) plus the two production
   constructors. Task A2 handles them with `matches!(..., { track_count: N, .. })` rather than
   asserting a destination that those tests are not about; destination correctness is asserted
   directly by Task A3's four path-specific tests. This is a small, recorded reduction in
   assertion strength on nine unrelated tests.
2. The spec describes `organize_file` as returning a `PathBuf` whose parent the caller derives.
   Because `organize_file` applies the same disc-subdirectory rule as the shared writer, that
   derivation would report the disc folder for a multi-disc album. Task A1 therefore unifies
   all three writers on `LibraryWriteOutcome` instead — same interface, no re-derivation.
