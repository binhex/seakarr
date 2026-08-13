# End-of-Run Summary Reporting — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Print a summary of Downloaded/Skipped/Failed albums to the console at the end of every run (auto, manual, batch, daemon per-cycle).

**Architecture:** New `src/report.rs` module holds `AlbumOutcome` enum and `RunReport` struct. `process_album` returns `Result<AlbumOutcome>` instead of `Result<()>`. Each mode runner collects outcomes into a `RunReport` and calls `print_summary()` at the end.

**Tech Stack:** Rust, tracing (existing), no new dependencies.

---

## File Structure

| File | Action | Responsibility |
| --- | --- | --- |
| `src/report.rs` | **Create** | `AlbumOutcome` enum, `RunReport` struct, `print_summary()` via `tracing::info!` |
| `src/lib.rs:13` | **Modify** | Add `pub mod report;` |
| `src/runner.rs:23-182` | **Modify** | Change `process_album` return type to `Result<AlbumOutcome>`, return variants at each exit point |
| `src/runner.rs:184-232` | **Modify** | `run_auto_mode`: collect outcomes into `RunReport`, call `print_summary()` |
| `src/runner.rs:234-248` | **Modify** | `run_manual_mode`: collect outcome into `RunReport`, call `print_summary()` |
| `src/main.rs:189-218` | **Modify** | `run_batch_mode`: replace counters with `RunReport`, call `print_summary()` |

---

### Task 1: Create `src/report.rs` with types and unit tests

**Files:**
- Create: `src/report.rs`
- Modify: `src/lib.rs:13`

- [ ] **Step 1: Write the failing test — `report.rs` unit tests**

Create `src/report.rs` with the types and comprehensive unit tests. The module will not compile until `lib.rs` declares it.

```rust
// src/report.rs

use tracing;

/// Outcome of processing a single album.
#[derive(Debug, Clone, PartialEq)]
pub enum AlbumOutcome {
    Downloaded { track_count: usize },
    Skipped,
    Failed { reason: String },
}

/// Collects album outcomes during a run and prints a summary.
#[derive(Debug, Default)]
pub struct RunReport {
    downloaded: Vec<(String, String)>,       // (artist, album)
    skipped: Vec<(String, String)>,          // (artist, album)
    failed: Vec<(String, String, String)>,   // (artist, album, reason)
}

impl RunReport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an album outcome.
    pub fn record(&mut self, artist: &str, album: &str, outcome: AlbumOutcome) {
        match outcome {
            AlbumOutcome::Downloaded { .. } => {
                self.downloaded.push((artist.to_string(), album.to_string()));
            }
            AlbumOutcome::Skipped => {
                self.skipped.push((artist.to_string(), album.to_string()));
            }
            AlbumOutcome::Failed { reason } => {
                self.failed.push((
                    artist.to_string(),
                    album.to_string(),
                    reason,
                ));
            }
        }
    }

    /// Print summary via tracing::info!. Omits empty sections. Prints nothing
    /// if no outcomes were recorded.
    pub fn print_summary(&self) {
        let total = self.downloaded.len() + self.skipped.len() + self.failed.len();
        if total == 0 {
            return;
        }

        tracing::info!("=== Run summary ===");

        if !self.downloaded.is_empty() {
            tracing::info!("Downloaded ({}):", self.downloaded.len());
            for (artist, album) in &self.downloaded {
                tracing::info!("  {artist} — {album}");
            }
        }

        if !self.skipped.is_empty() {
            tracing::info!("Skipped ({}):", self.skipped.len());
            for (artist, album) in &self.skipped {
                tracing::info!("  {artist} — {album}");
            }
        }

        if !self.failed.is_empty() {
            tracing::info!("Failed ({}):", self.failed.len());
            for (artist, album, reason) in &self.failed {
                tracing::info!("  {artist} — {album} ({reason})");
            }
        }
    }

    // Accessors for testing
    pub fn downloaded_count(&self) -> usize {
        self.downloaded.len()
    }

    pub fn skipped_count(&self) -> usize {
        self.skipped.len()
    }

    pub fn failed_count(&self) -> usize {
        self.failed.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_report_has_no_outcomes() {
        let report = RunReport::new();
        assert_eq!(report.downloaded_count(), 0);
        assert_eq!(report.skipped_count(), 0);
        assert_eq!(report.failed_count(), 0);
    }

    #[test]
    fn test_record_downloaded() {
        let mut report = RunReport::new();
        report.record("Artist A", "Album 1", AlbumOutcome::Downloaded { track_count: 10 });
        assert_eq!(report.downloaded_count(), 1);
        assert_eq!(report.skipped_count(), 0);
        assert_eq!(report.failed_count(), 0);
    }

    #[test]
    fn test_record_skipped() {
        let mut report = RunReport::new();
        report.record("Artist B", "Album 2", AlbumOutcome::Skipped);
        assert_eq!(report.downloaded_count(), 0);
        assert_eq!(report.skipped_count(), 1);
        assert_eq!(report.failed_count(), 0);
    }

    #[test]
    fn test_record_failed() {
        let mut report = RunReport::new();
        report.record(
            "Artist C",
            "Album 3",
            AlbumOutcome::Failed {
                reason: "all candidates exhausted".into(),
            },
        );
        assert_eq!(report.downloaded_count(), 0);
        assert_eq!(report.skipped_count(), 0);
        assert_eq!(report.failed_count(), 1);
    }

    #[test]
    fn test_mixed_outcomes() {
        let mut report = RunReport::new();
        report.record("A", "1", AlbumOutcome::Downloaded { track_count: 5 });
        report.record("B", "2", AlbumOutcome::Skipped);
        report.record("C", "3", AlbumOutcome::Failed { reason: "timeout".into() });
        report.record("D", "4", AlbumOutcome::Downloaded { track_count: 8 });
        assert_eq!(report.downloaded_count(), 2);
        assert_eq!(report.skipped_count(), 1);
        assert_eq!(report.failed_count(), 1);
    }

    #[test]
    fn test_ordering_preserved() {
        let mut report = RunReport::new();
        report.record("Z", "first", AlbumOutcome::Downloaded { track_count: 1 });
        report.record("A", "second", AlbumOutcome::Skipped);
        // Entries should be in the order they were recorded.
        assert_eq!(report.downloaded[0], ("Z".to_string(), "first".to_string()));
        assert_eq!(report.skipped[0], ("A".to_string(), "second".to_string()));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib report -- --nocapture 2>&1 | head -20`
Expected: compilation error — module `report` not found in `lib.rs`.

- [ ] **Step 3: Register module in `lib.rs`**

Add `pub mod report;` to `src/lib.rs` after line 12 (`pub mod tracks;`):

```rust
// src/lib.rs
pub mod client;
pub mod config;
pub mod db;
pub mod download;
pub mod error;
pub mod filter;
pub mod notifier;
pub mod organizer;
pub mod report;
pub mod runner;
pub mod scanner;
pub mod search;
pub mod tracks;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib report -- --nocapture`
Expected: all 6 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/report.rs src/lib.rs
git commit -m "feat: add RunReport and AlbumOutcome types with unit tests"
```

---

### Task 2: Change `process_album` return type to `Result<AlbumOutcome>`

**Files:**
- Modify: `src/runner.rs:23-182`
- Modify: `src/runner.rs:234-248` (run_manual_mode — must handle new return type)

- [ ] **Step 1: Write the failing test — update existing runner tests**

The existing tests in `src/runner.rs` call `process_album` and assert `result.is_ok()`. After the return type changes, they must also assert the specific `AlbumOutcome` variant. Update the test assertions:

In `test_run_manual_mode` (line ~298):
```rust
    #[tokio::test]
    async fn test_run_manual_mode() {
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file("01 - track.flac", 900, 10_000_000)],
        }];

        let config = make_test_config();
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), AlbumOutcome::Downloaded { track_count: 1 });
    }
```

In `test_fallback_disabled_by_config_issues_single_search` (line ~324):
```rust
        // ... (all existing setup unchanged) ...
        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), AlbumOutcome::Skipped);

        // ... rest of assertions unchanged ...
```

In `test_fallback_download_completes_album_and_records_history` (line ~356):
```rust
        // ... (all existing setup unchanged) ...
        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), AlbumOutcome::Downloaded { track_count: 1 });

        // ... rest of assertions unchanged ...
```

In `test_second_chance_fallback_when_primary_all_rejected` (line ~410):
```rust
        // ... (all existing setup unchanged) ...
        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), AlbumOutcome::Downloaded { track_count: 2 });

        // ... rest of assertions unchanged ...
```

In `test_fallback_no_matches_marks_skipped` (line ~458):
```rust
        // ... (all existing setup unchanged) ...
        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), AlbumOutcome::Skipped);

        // ... rest of assertions unchanged ...
```

In `test_fallback_with_gappy_tracks_marks_skipped` (line ~498):
```rust
        // ... (all existing setup unchanged) ...
        let result = process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), AlbumOutcome::Skipped);

        // ... rest of assertions unchanged ...
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib runner -- --nocapture 2>&1 | head -30`
Expected: compilation error — `process_album` returns `Result<()>` but test expects `AlbumOutcome`.

- [ ] **Step 3: Change `process_album` signature and all return points**

In `src/runner.rs`, change the function signature (line 23) and add the import:

```rust
use crate::report::AlbumOutcome;
```

Change the signature:
```rust
pub async fn process_album(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    config: &Config,
    db: &Database,
    staging_dir: &Path,
) -> Result<AlbumOutcome> {
```

Change each return point:

1. **Already-processed skip** (line ~33): `return Ok(());` → `return Ok(AlbumOutcome::Skipped);`

2. **No search results** (line ~86): `return Ok(());` → `return Ok(AlbumOutcome::Skipped);`

3. **Zero passed filters** (line ~116): `return Ok(());` → `return Ok(AlbumOutcome::Skipped);`

4. **Download failed** (lines ~131-135): Change from `return Err(e);` to:
```rust
            tracing::warn!(
                "{artist} — {}: download failed ({e}); {} candidates exhausted",
                album.unwrap_or("(all)"),
                ranked.len(),
            );
            return Ok(AlbumOutcome::Failed {
                reason: format!("all candidates exhausted: {e}"),
            });
```

5. **Organize failed** (lines ~164-168): Change from `return Err(SeakarrError::Download(...));` to:
```rust
    if !organize_ok {
        return Ok(AlbumOutcome::Failed {
            reason: "download succeeded but file organization failed".into(),
        });
    }
```

6. **Success** (line ~177): Change from `Ok(())` to:
```rust
    Ok(AlbumOutcome::Downloaded { track_count })
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib runner -- --nocapture`
Expected: all runner tests PASS with the new `AlbumOutcome` assertions.

- [ ] **Step 5: Commit**

```bash
git add src/runner.rs
git commit -m "feat: process_album returns AlbumOutcome instead of Result<()>"
```

---

### Task 3: Update `run_auto_mode` to collect and print summary

**Files:**
- Modify: `src/runner.rs:184-232` (run_auto_mode)

- [ ] **Step 1: Write a test that exercises the full auto-mode path**

Add this test at the end of the `mod tests` block in `src/runner.rs`:

```rust
    #[tokio::test]
    async fn test_run_auto_mode_processes_album_and_marks_success() {
        let client = Arc::new(MockClient::new());
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file("01 - track.flac", 900, 10_000_000)],
        }];

        let mut config = make_test_config();
        let tmp = TempDir::new().unwrap();
        // Library layout: <tmp>/Test Artist/Test Album/01 - track.mp3
        // mp3 is not in allowed_extensions (default [flac]) so the album is
        // flagged for upgrade; the mock search supplies the flac result.
        let artist_dir = tmp.path().join("Test Artist").join("Test Album");
        std::fs::create_dir_all(&artist_dir).unwrap();
        std::fs::write(artist_dir.join("01 - track.mp3"), b"fake mp3 data").unwrap();
        config.library.paths = vec![tmp.path().to_string_lossy().into()];

        let db = Database::open_in_memory().unwrap();

        let result = run_auto_mode(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            &config,
            &db,
        )
        .await;
        assert!(result.is_ok());

        // Album processed successfully through the outcome-collection path.
        let rows = db.get_processed_albums().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "success");
    }
```

- [ ] **Step 2: Run test to verify it fails (compile error)**

Run: `cargo test --lib runner::tests::test_run_auto_mode_processes_album_and_marks_success -- --nocapture`
Expected: compilation error — the future type mismatch (process_album returns `Result<()>`, not yet `Result<AlbumOutcome>`); the test cannot compile until `run_auto_mode` is rewritten.

- [ ] **Step 3: Update `run_auto_mode` to collect outcomes into `RunReport`**

Add the import at the top of `src/runner.rs` (next to the existing `use crate::{...}`):

```rust
use crate::report::{AlbumOutcome, RunReport};
```

Replace the entire concurrent-processing section of `run_auto_mode` (from `let staging_dir = ...` after the empty-targets early return, to the end of the function) with:

```rust
    // Process concurrently with bounded concurrency.
    //
    // NOTE: `tokio::spawn` cannot be used here — the borrowed `&Database` is
    // !Send (rusqlite::Connection is not Sync), and spawn requires 'static
    // futures. Instead we build !Send boxed local futures that borrow
    // `client`/`config`/`db` and poll them cooperatively in this task via
    // `join_all` (FuturesUnordered under the hood), bounding the number of
    // albums in flight with a shared tokio semaphore.
    let staging_dir = Path::new(&config.storage.staging_dir);
    std::fs::create_dir_all(staging_dir)?;

    let semaphore = Arc::new(Semaphore::new(config.download.concurrent.max(1)));

    let targets_vec: Vec<(String, String)> = targets;
    let mut futures_vec = Vec::new();

    for (artist, album) in &targets_vec {
        let semaphore = Arc::clone(&semaphore);
        let artist = artist.clone();
        let album = album.clone();
        futures_vec.push(async move {
            // Park until a permit is free — this is what bounds concurrency.
            let _permit = semaphore
                .acquire_owned()
                .await
                .expect("semaphore is never closed");
            let result =
                process_album(client, &artist, Some(&album), config, db, staging_dir).await;
            (artist, album, result)
        }
        .boxed_local());
    }

    let results = futures::future::join_all(futures_vec).await;

    // Collect outcomes into the run report and print the summary once at the
    // end. Environment errors (staging dir, DB write, notify failure) are
    // recorded as Failed entries so they appear in the summary.
    let mut report = RunReport::new();
    for (artist, album, result) in results {
        match result {
            Ok(outcome) => report.record(&artist, &album, outcome),
            Err(e) => {
                tracing::error!("Album processing failed: {artist} — {album}: {e}");
                report.record(&artist, &album, AlbumOutcome::Failed {
                    reason: e.to_string(),
                });
            }
        }
    }
    report.print_summary();

    Ok(())
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib runner -- --nocapture`
Expected: all tests PASS, including the new auto-mode test.

- [ ] **Step 5: Commit**

```bash
git add src/runner.rs
git commit -m "feat: run_auto_mode collects outcomes into RunReport and prints summary"
```

---

### Task 4: Update `run_manual_mode` to print summary

**Files:**
- Modify: `src/runner.rs:234-248` (run_manual_mode)

- [ ] **Step 1: Update `run_manual_mode`**

Replace the function body:

```rust
/// Run in manual mode: process a single search term.
pub async fn run_manual_mode(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    config: &Config,
    db: &Database,
) -> Result<()> {
    let staging_dir = Path::new(&config.storage.staging_dir);
    std::fs::create_dir_all(staging_dir)?;

    let album_display = album.unwrap_or("(all)");
    let mut report = RunReport::new();

    // Environment errors (staging dir creation, DB write, notify failure)
    // must still propagate so the CLI exits non-zero — record them as
    // Failed entries first so they appear in the summary.
    let result = process_album(client, artist, album, config, db, staging_dir).await;
    match &result {
        Ok(outcome) => report.record(artist, album_display, outcome.clone()),
        Err(e) => {
            tracing::error!("Manual mode: {artist} — {album_display}: {e}");
            report.record(artist, album_display, AlbumOutcome::Failed {
                reason: e.to_string(),
            });
        }
    }

    report.print_summary();
    result.map(|_| ())
}
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test --lib runner -- --nocapture`
Expected: all tests PASS.

- [ ] **Step 3: Commit**

```bash
git add src/runner.rs
git commit -m "feat: run_manual_mode prints summary via RunReport"
```

---

### Task 5: Update `run_batch_mode` in `main.rs` to print summary

**Files:**
- Modify: `src/main.rs:189-218` (run_batch_mode)

- [ ] **Step 1: Update `run_batch_mode`**

Replace the function body:

```rust
/// Batch mode: process a newline-separated list of `artist - album` lines.
async fn run_batch_mode(
    client: &dyn SoulseekClient,
    batch_path: &str,
    config: &Config,
    db: &Database,
) -> Result<()> {
    let contents = std::fs::read_to_string(batch_path)?;
    let lines: Vec<&str> = contents
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    tracing::info!("Batch mode: {} lines to process", lines.len());
    let staging_dir = Path::new(&config.storage.staging_dir);
    std::fs::create_dir_all(staging_dir)?;

    let mut report = seakarr::report::RunReport::new();

    for line in &lines {
        let parts: Vec<&str> = line.splitn(2, " - ").collect();
        let artist = parts[0].trim();
        let album = parts.get(1).map(|a| a.trim()).filter(|a| !a.is_empty());
        let album_display = album.unwrap_or("(all)");

        match seakarr::runner::process_album(client, artist, album, config, db, staging_dir).await {
            Ok(outcome) => report.record(artist, album_display, outcome),
            Err(e) => {
                tracing::error!("Batch: failed {artist} — {album_display}: {e}");
                report.record(artist, album_display, seakarr::report::AlbumOutcome::Failed {
                    reason: e.to_string(),
                });
            }
        }
    }

    report.print_summary();
    Ok(())
}
```

- [ ] **Step 2: Run full test suite**

Run: `cargo test --lib -- --nocapture 2>&1 | tail -20`
Expected: all tests PASS.

- [ ] **Step 3: Run clippy and format check**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: no warnings or errors.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: run_batch_mode prints summary via RunReport"
```

---

### Task 6: Final verification and integration test

**Files:**
- Test: `tests/` (existing integration tests)

- [ ] **Step 1: Run full test suite**

Run: `cargo test 2>&1 | tail -30`
Expected: all tests PASS.

- [ ] **Step 2: Run clippy and format**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Build release**

Run: `cargo build --release 2>&1 | tail -5`
Expected: successful compilation.

- [ ] **Step 4: Commit any fixes**

```bash
git add -A
git commit -m "chore: clippy/fmt fixes for end-of-run summary"
```

---

## Verification Checklist

- [ ] `cargo test` — all tests pass
- [ ] `cargo fmt --check` — no formatting issues
- [ ] `cargo clippy --all-targets -- -D warnings` — no warnings
- [ ] `cargo build --release` — compiles cleanly
- [ ] Manual mode prints summary after single album
- [ ] Batch mode prints summary after all lines
- [ ] Auto mode prints summary after scan cycle
- [ ] Daemon mode prints summary per cycle (each cycle calls `run_auto_mode`)
- [ ] Empty sections are omitted from output
- [ ] No outcomes = no summary printed
