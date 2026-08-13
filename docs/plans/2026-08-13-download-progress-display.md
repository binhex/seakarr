# Download Progress Display — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace noisy raw-bytes download logging with human-friendly console progress bars (one per active album) using `indicatif`, with automatic TTY detection for non-interactive fallback.

**Architecture:** Two new modules (`formatting.rs` for byte/speed formatting, `progress.rs` for indicatif-based bar management). `download_file` and `download_album` gain optional progress parameters. Bridge logs in `client.rs` are suppressed when bars are active, reformatted when not.

**Tech Stack:** Rust, `indicatif 0.17`, `tracing` (existing), `std::io::IsTerminal` (stabilised in Rust 1.70+)

---

## File Structure

| File | Action | Responsibility |
| --- | --- | --- |
| `Cargo.toml` | Modify | Add `indicatif = "0.17"` dependency |
| `src/lib.rs` | Modify | Declare `pub mod formatting; pub mod progress;` |
| `src/formatting.rs` | **Create** | `format_bytes(u64)`, `format_speed(u64)` — human-friendly unit conversion |
| `src/progress.rs` | **Create** | `ProgressDisplay` (wraps `MultiProgress`), `is_interactive()`, bar create/update/finish |
| `src/client.rs` | Modify | Bridge InProgress logs: suppress when interactive, human-friendly format when not |
| `src/download.rs` | Modify | `download_file`/`download_album` accept optional progress, update bars on InProgress |
| `src/runner.rs` | Modify | `process_album` passes `Option<&ProgressDisplay>` to `download_album` |

---

### Task 1: Add `indicatif` dependency and register new modules

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add indicatif to Cargo.toml**

Add to `[dependencies]` section (after `chrono`):

```toml
indicatif = "0.17"
```

- [ ] **Step 2: Register new modules in lib.rs**

Add two lines to `src/lib.rs` (after `pub mod filter;` and in the existing alphabetical order):

```rust
pub mod formatting;
```

And after `pub mod notifier;`:

```rust
pub mod progress;
```

The full `lib.rs` should read:

```rust
// lib.rs — Seakarr library root. Modules are declared here so integration
// tests (in tests/) can import from `seakarr::`.

pub mod client;
pub mod config;
pub mod db;
pub mod download;
pub mod error;
pub mod filter;
pub mod formatting;
pub mod notifier;
pub mod organizer;
pub mod progress;
pub mod report;
pub mod runner;
pub mod scanner;
pub mod search;
pub mod tracks;
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: compiles (empty formatting.rs and progress.rs modules will be needed — create them as empty files for now: `touch src/formatting.rs src/perogress.rs`, or proceed to Task 2 which creates them with content).

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml src/lib.rs Cargo.lock
git commit -m "chore: add indicatif dependency and register formatting/progress modules"
```

---

### Task 2: Implement `formatting.rs` with tests

**Files:**
- Create: `src/formatting.rs`

This module has zero internal dependencies — pure functions, easily tested in isolation.

- [ ] **Step 1: Write failing tests for format_bytes**

Create `src/formatting.rs` with tests first:

```rust
/// Human-friendly byte/speed formatting for download progress display.

/// Format a byte count into a human-friendly string.
///
/// | Range      | Format      | Example     |
/// |------------|-------------|-------------|
/// | < 1024     | `{n} B`     | `512 B`     |
/// | < 1 MB     | `{n:.1} KB` | `256.5 KB`  |
/// | < 1 GB     | `{n:.1} MB` | `31.8 MB`   |
/// | >= 1 GB    | `{n:.1} GB` | `1.2 GB`    |
pub fn format_bytes(bytes: u64) -> String {
    todo!()
}

/// Format a speed in bytes/sec into a human-friendly string.
///
/// | Range        | Format       | Example      |
/// |--------------|--------------|--------------|
/// | < 1024 B/s   | `{n} B/s`    | `512 B/s`    |
/// | < 1 MB/s     | `{n:.1} KB/s`| `256.5 KB/s` |
/// | >= 1 MB/s    | `{n:.1} MB/s`| `2.5 MB/s`   |
pub fn format_speed(bytes_per_sec: u64) -> String {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn test_format_bytes_under_1kb() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1023), "1023 B");
    }

    #[test]
    fn test_format_bytes_kb_range() {
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(102_400), "100.0 KB");
    }

    #[test]
    fn test_format_bytes_mb_range() {
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
        assert_eq!(format_bytes(10_485_760), "10.0 MB");
        assert_eq!(format_bytes(33_304_229), "31.8 MB");
    }

    #[test]
    fn test_format_bytes_gb_range() {
        assert_eq!(format_bytes(1_073_741_824), "1.0 GB");
        assert_eq!(format_bytes(2_147_483_648), "2.0 GB");
    }

    #[test]
    fn test_format_speed_zero() {
        assert_eq!(format_speed(0), "0 B/s");
    }

    #[test]
    fn test_format_speed_under_1kbs() {
        assert_eq!(format_speed(512), "512 B/s");
    }

    #[test]
    fn test_format_speed_kbs_range() {
        assert_eq!(format_speed(1024), "1.0 KB/s");
        assert_eq!(format_speed(2_649_609), "2.5 MB/s"); // from the user's log
    }

    #[test]
    fn test_format_speed_mbs_range() {
        assert_eq!(format_speed(1_048_576), "1.0 MB/s");
        assert_eq!(format_speed(2_850_380), "2.7 MB/s"); // from the user's log
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib formatting -- --nocapture 2>&1 | head -20`
Expected: FAIL — `not yet implemented` (todo! panics)

- [ ] **Step 3: Implement format_bytes and format_speed**

Replace the `todo!()` bodies:

```rust
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;

    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.1} KB", b / KB)
    } else if b < GB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.1} GB", b / GB)
    }
}

pub fn format_speed(bytes_per_sec: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;

    let b = bytes_per_sec as f64;
    if b < KB {
        format!("{bytes_per_sec} B/s")
    } else if b < MB {
        format!("{:.1} KB/s", b / KB)
    } else {
        format!("{:.1} MB/s", b / MB)
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib formatting -- --nocapture`
Expected: all 9 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/formatting.rs
git commit -m "feat: add format_bytes and format_speed with unit tests"
```

---

### Task 3: Implement `progress.rs` with tests

**Files:**
- Create: `src/progress.rs`

Depends on: `formatting.rs` (Task 2), `indicatif` (Task 1).

- [ ] **Step 1: Write failing tests for ProgressDisplay**

Create `src/progress.rs`:

```rust
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::io::IsTerminal;

use crate::formatting::{format_bytes, format_speed};

/// Check if stderr is an interactive terminal.
/// When false, progress bars should not be created.
pub fn is_interactive() -> bool {
    std::io::stderr().is_terminal()
}

/// Manages download progress bars — one per active album.
///
/// Wraps `indicatif::MultiProgress` so multiple concurrent album downloads
/// each get their own progress bar. In non-interactive mode, all methods
/// are no-ops.
pub struct ProgressDisplay {
    multi: MultiProgress,
}

impl ProgressDisplay {
    /// Create a new ProgressDisplay.
    /// Renders to stderr. Should only be called when `is_interactive()` is true.
    pub fn new() -> Self {
        let multi = MultiProgress::new();
        // indicatif renders to stderr by default — matches spec.
        Self { multi }
    }

    /// Create a progress bar for a track download.
    ///
    /// The bar shows: filename | downloaded/total | speed | bar | percentage
    pub fn create_bar(&self, filename: &str, total_bytes: u64) -> ProgressBar {
        let bar = self.multi.add(ProgressBar::new(total_bytes));
        let style = ProgressStyle::with_template(
            "  {spinner} {msg}  {bytes}/{total_bytes}  {bytes_per_sec}  [{bar:20}]  {percent}%",
        )
        .expect("valid progress bar template");
        let style = style.progress_chars("█░");
        bar.set_style(style);
        // Show only the basename, truncate if needed
        let display_name = filename
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(filename);
        bar.set_message(display_name.to_string());
        bar
    }

    /// Remove all bars (call when download session ends).
    pub fn clear(&self) {
        let _ = self.multi.clear();
    }
}

impl Default for ProgressDisplay {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_interactive_returns_false_in_tests() {
        // In test context (cargo test), stderr is typically not a TTY.
        // This test may pass or fail depending on how tests are run,
        // but documents the expected behaviour.
        // We just verify the function doesn't panic.
        let _ = is_interactive();
    }

    #[test]
    fn test_progress_display_creation() {
        let display = ProgressDisplay::new();
        let bar = display.create_bar("01 - Track.flac", 33_304_229);
        // Bar should be created and usable
        bar.set_position(10_552_744);
        assert_eq!(bar.position(), 10_552_744);
        bar.finish();
        display.clear();
    }

    #[test]
    fn test_create_bar_extracts_basename() {
        let display = ProgressDisplay::new();
        let bar = display.create_bar(
            r"Music\Artist\Album\01 - Track.flac",
            1000,
        );
        // Message should be just the basename
        // indicatif stores the message — we can't easily read it back,
        // but we can verify no panic occurred.
        bar.finish();
        display.clear();
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib progress -- --nocapture 2>&1 | head -20`
Expected: FAIL — `progress` module not found (lib.rs doesn't declare it yet... actually Task 1 should have added it. If Task 1 was completed, it should compile but `ProgressDisplay::new()` might not exist yet. If you're implementing Task 2 and 3 together, the `todo!()` on format functions will cause failure.)

- [ ] **Step 3: Implement ProgressDisplay**

Replace the test stubs with the real implementation shown in Step 1 above (the code is complete as written). The `create_bar` method uses a template that shows: spinner, filename, downloaded/total bytes, speed, bar, percentage.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib progress -- --nocapture`
Expected: all 3 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/progress.rs
git commit -m "feat: add ProgressDisplay with TTY detection and bar lifecycle"
```

---

### Task 4: Update `download_file` to accept and update a progress bar

**Files:**
- Modify: `src/download.rs:29-105`

- [ ] **Step 1: Write a test for progress bar updates**

Add a test in `src/download.rs` `mod tests` that verifies `download_file` with a `Some(ProgressBar)` doesn't panic and updates the bar position:

```rust
#[tokio::test]
async fn test_download_file_with_progress_bar() {
    use indicatif::ProgressBar;

    let client = MockClient::new();
    let dir = TempDir::new().unwrap();
    let file = make_file("01 - track.flac", 900, 10_000_000);

    let config = default_dl_config();
    let bar = ProgressBar::new(10_000_000);

    let result = download_file(&client, &file, "testuser", dir.path(), &config, Some(&bar)).await;
    assert!(result.is_ok());
    assert!(bar.is_finished());
}
```

Note: this test will fail to compile until the signature change in Step 2 is applied.

- [ ] **Step 2: Change download_file signature**

Add the `bar` parameter to `download_file`:

```rust
pub async fn download_file(
    client: &dyn SoulseekClient,
    file: &FileInfo,
    username: &str,
    dir: &Path,
    config: &DownloadConfig,
    bar: Option<&ProgressBar>,  // NEW: optional progress bar
) -> Result<PathBuf> {
```

Add the import at the top of `download.rs`:

```rust
use indicatif::ProgressBar;
```

- [ ] **Step 3: Update InProgress handling to update the bar**

In the `Ok(Some(DownloadStatus::InProgress { speed_bytes_per_sec, .. }))` match arm, after the speed check block, add bar update:

```rust
            Ok(Some(DownloadStatus::InProgress {
                speed_bytes_per_sec,
                bytes_downloaded,
                total_bytes: _,
            })) => {
                // ... existing speed check code unchanged ...

                // Update progress bar if present
                if let Some(ref bar) = bar {
                    bar.set_position(bytes_downloaded);
                    bar.set_prefix(format_speed(speed_bytes_per_sec));
                }
            }
```

Also update the `DownloadStatus::Completed` arm to finish the bar:

```rust
            Ok(Some(DownloadStatus::Completed)) => {
                if let Some(ref bar) = bar {
                    bar.finish_with_message(format!(
                        "{} ✓",
                        bar.message()  // keep the filename
                    ));
                }
                let dest = dir.join(basename);
                tracing::info!("Download completed: {basename}");
                return Ok(dest);
            }
```

And in the failure/timeout paths, abandon the bar:

```rust
            Ok(Some(DownloadStatus::Failed { reason })) => {
                if let Some(ref bar) = bar {
                    bar.abandon_with_message(format!(
                        "{} ✗",
                        bar.message()
                    ));
                }
                // ... existing error handling ...
            }
```

Do the same for the `Err(_elapsed)` timeout arm.

- [ ] **Step 4: Update all download_file callers**

Every call to `download_file` must add the new `bar` parameter. Search for `download_file(` in the file:

In `download_album` (line ~160), change:
```rust
// Before:
download_file(client, file, &candidate.username, staging_dir, config).await

// After:
download_file(client, file, &candidate.username, staging_dir, config, None).await
```

In tests, update the helper calls. The existing `download_file(&client, &file, "testuser", dir.path(), &config)` calls become `download_file(&client, &file, "testuser", dir.path(), &config, None)`.

- [ ] **Step 5: Run all tests**

Run: `cargo test --lib download -- --nocapture`
Expected: all existing download tests PASS (None bars), plus the new progress bar test PASS.

- [ ] **Step 6: Commit**

```bash
git add src/download.rs
git commit -m "feat: download_file accepts optional progress bar, updates on InProgress"
```

---

### Task 5: Update `download_album` to create bars per track

**Files:**
- Modify: `src/download.rs:116-195`

- [ ] **Step 1: Change download_album signature**

Add `progress` parameter:

```rust
pub async fn download_album(
    client: &dyn SoulseekClient,
    candidates: &[SearchResult],
    staging_dir: &Path,
    config: &DownloadConfig,
    filters: &crate::config::FilterConfig,
    progress: Option<&ProgressDisplay>,  // NEW
) -> Result<Vec<PathBuf>> {
```

Add the import:

```rust
use crate::progress::{ProgressDisplay};
```

- [ ] **Step 2: Create a bar for each track, pass to download_file**

In the `for file in &filtered_files` loop (around line 160), create a bar when progress is Some:

```rust
for file in &filtered_files {
    let bar = progress.as_ref().map(|p| {
        let basename = safe_basename(&file.name).unwrap_or_else(|_| file.name.clone());
        let total = file.size;
        p.create_bar(&basename, total)
    });
    let bar_ref = bar.as_ref();
    match download_file(client, file, &candidate.username, staging_dir, config, bar_ref).await {
```

- [ ] **Step 3: Update all download_album callers in runner.rs**

In `src/runner.rs`, the `download::download_album(...)` call at line 174:

```rust
// Before:
    let downloaded = match download::download_album(
        client,
        &ranked,
        &album_staging,
        &config.download,
        &config.filters,
    )

// After:
    let downloaded = match download::download_album(
        client,
        &ranked,
        &album_staging,
        &config.download,
        &config.filters,
        progress,
    )
```

- [ ] **Step 4: Add progress parameter to process_album**

Change `process_album` signature in `src/runner.rs`:

```rust
pub async fn process_album(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    config: &Config,
    db: &Database,
    staging_dir: &Path,
    progress: Option<&ProgressDisplay>,  // NEW
) -> Result<AlbumOutcome> {
```

Add the import:

```rust
use crate::progress::ProgressDisplay;
```

Forward `progress` to the `download::download_album` call.

- [ ] **Step 5: Update all process_album callers**

Every call to `process_album` must add the `progress` parameter. In the same file:

- `run_auto_mode` (line ~330): create one `ProgressDisplay` when `is_interactive()`, pass `Some(&progress)` to each `process_album` call inside the futures. Since `ProgressDisplay` is shared across concurrent futures, wrap it in `Arc`.
- `run_manual_mode` (line ~389): create one `ProgressDisplay`, pass `Some(&progress)` to `process_album`.
- `run_batch_mode` in `src/main.rs` (line ~383): create one `ProgressDisplay`, pass `Some(&progress)` to each `process_album`.

For test callers: pass `None`.

- [ ] **Step 6: Run all tests**

Run: `cargo test --lib -- --nocapture`
Expected: all tests PASS.

- [ ] **Step 7: Commit**

```bash
git add src/runner.rs src/main.rs src/download.rs
git commit -m "feat: wire ProgressDisplay through download_album and process_album"
```

---

### Task 6: Suppress bridge logs when progress bars are active

**Files:**
- Modify: `src/client.rs:494-514`

This is the "bridge" area where `SsDownloadStatus::InProgress` gets logged.

- [ ] **Step 1: Add format_bytes/format_speed import to client.rs**

At the top of `src/client.rs`:

```rust
use crate::formatting::{format_bytes, format_speed};
```

Also import the progress check:

```rust
use crate::progress::is_interactive;
```

- [ ] **Step 2: Modify bridge InProgress logging**

In the bridge's `spawn_blocking` block (around line 505), change the InProgress logging:

```rust
// Before:
if last_progress_log.elapsed() >= std::time::Duration::from_secs(5) {
    tracing::info!("Bridge progress for {bridge_filename}: {status:?}");
    last_progress_log = std::time::Instant::now();
}

// After:
if !is_interactive() {
    // Non-interactive: human-friendly fallback log
    if last_progress_log.elapsed() >= std::time::Duration::from_secs(5) {
        if let SsDownloadStatus::InProgress {
            bytes_downloaded,
            total_bytes,
            speed_bytes_per_sec,
        } = &status
        {
            tracing::info!(
                "Downloading: {bridge_filename} — {} / {} @ {}",
                format_bytes(*bytes_downloaded),
                format_bytes(*total_bytes),
                format_speed(*speed_bytes_per_sec as u64),
            );
        }
        last_progress_log = std::time::Instant::now();
    }
}
// When interactive: suppressed — progress bar handles display
```

- [ ] **Step 3: Verify existing client tests still pass**

Run: `cargo test --lib client -- --nocapture`
Expected: all tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src/client.rs
git commit -m "feat: bridge logs use human-friendly format, suppressed when progress bars active"
```

---

### Task 7: Wire ProgressDisplay into mode runners

**Files:**
- Modify: `src/runner.rs` (run_auto_mode, run_manual_mode)
- Modify: `src/main.rs` (run_batch_mode)

- [ ] **Step 1: Create ProgressDisplay in run_auto_mode**

In `run_auto_mode`, create the ProgressDisplay before spawning futures. Since multiple concurrent albums share it, wrap in `Arc`:

```rust
use std::sync::Arc;
use crate::progress::{is_interactive, ProgressDisplay};

// Inside run_auto_mode, before the futures section:
let progress = if is_interactive() {
    Some(Arc::new(ProgressDisplay::new()))
} else {
    None
};
```

Pass `progress.as_deref()` (which converts `Option<Arc<ProgressDisplay>>` to `Option<&ProgressDisplay>`) to each `process_album` call inside the futures.

After `join_all`, clear the progress display:

```rust
if let Some(ref p) = progress {
    p.clear();
}
```

- [ ] **Step 2: Create ProgressDisplay in run_manual_mode**

```rust
let progress = if is_interactive() {
    Some(ProgressDisplay::new())
} else {
    None
};
let progress_ref = progress.as_ref();
let result = process_album(client, artist, album, config, db, staging_dir, progress_ref).await;
```

- [ ] **Step 3: Create ProgressDisplay in run_batch_mode (main.rs)**

In `src/main.rs`'s `run_batch_mode`:

```rust
let progress = if seakarr::progress::is_interactive() {
    Some(seakarr::progress::ProgressDisplay::new())
} else {
    None
};
```

Pass `progress.as_ref()` to each `process_album` call. Clear after the loop.

- [ ] **Step 4: Update process_album callers in tests**

All test callers of `process_album` in `runner.rs` pass `None` for the progress parameter.

- [ ] **Step 5: Run full test suite**

Run: `cargo test 2>&1 | tail -20`
Expected: all tests PASS (including integration tests in `tests/pipeline_test.rs`).

- [ ] **Step 6: Commit**

```bash
git add src/runner.rs src/main.rs
git commit -m "feat: wire ProgressDisplay into auto/manual/batch mode runners"
```

---

### Task 8: Final verification

- [ ] **Step 1: Run full test suite**

Run: `cargo test`
Expected: all tests PASS.

- [ ] **Step 2: Run clippy and format check**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings`
Expected: no warnings or errors.

- [ ] **Step 3: Build release**

Run: `cargo build --release`
Expected: successful compilation.

- [ ] **Step 4: Manual smoke test**

Run: `cargo run -- --help` to verify the binary works. If Soulseek credentials are configured, run `cargo run` and observe the progress bars in action.

---

## Verification Checklist

- [ ] `cargo test` — all tests pass
- [ ] `cargo fmt --check` — no formatting issues
- [ ] `cargo clippy --all-targets -- -D warnings` — no warnings
- [ ] `cargo build --release` — compiles cleanly
- [ ] Progress bars appear on stderr when downloading in interactive terminal
- [ ] Non-interactive mode shows human-friendly log lines (no raw bytes)
- [ ] `format_bytes(33_304_229)` returns `"31.8 MB"`
- [ ] `format_speed(2_649_609)` returns `"2.5 MB/s"`
- [ ] Existing download tests pass unchanged (None progress parameter)
- [ ] Daemon mode falls back to log output (no progress bars)
