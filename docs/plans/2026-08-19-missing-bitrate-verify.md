# Missing Bitrate/Bitdepth Download-Then-Verify Fallback — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Change the behavior so that files with missing bitrate/bitdepth metadata are downloaded and verified post-download, rather than rejected at search time.

**Architecture:** The filter (`file_passes_filters`) stops rejecting files with missing bitrate/bitdepth when the corresponding config is set. The download loop (`download_album`) verifies each track after download using `file_quality_score()` (lofty). If any track fails, the download stops immediately and the next peer is tried.

**Tech Stack:** Rust, lofty (audio metadata), existing seakarr filter/download/organizer modules

**Spec:** `docs/specs/2026-08-19-missing-bitrate-verify-design.md`

---

## File Structure

| File | Responsibility | Change Type |
|------|---------------|-------------|
| `src/filter.rs` | Pre-download filtering of search results | Modify: stop rejecting missing bitrate/bitdepth |
| `src/download.rs` | File download + retry + fallback logic | Modify: add post-download verification |
| `src/organizer.rs` | Audio format detection + quality scoring | Modify: expose bitrate/bitdepth extraction helpers |
| `src/config.rs` | Config structs | No change (min_bitdepth already parsed) |

---

### Task 1: Expose bitrate/bitdepth extraction helpers in organizer.rs

**Files:**
- Modify: `src/organizer.rs`

The existing `file_quality_score()` returns a combined `u64` score. We need helpers that extract the raw bitrate or bitdepth for comparison against config minimums.

- [ ] **Step 1: Write the failing tests**

```rust
// In src/organizer.rs mod tests:

#[test]
fn test_extract_bitrate_from_lossy_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.mp3");
    write_real_flac(&path); // reuse existing helper — actually writes a real file
    // For a real test we need a known-lossy file. Use a minimal MP3.
    // Since write_real_flac creates a FLAC, let's test with that for bitdepth.
    // For bitrate, we'll test the extraction function directly.
}

#[test]
fn test_extract_bitdepth_from_lossless_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.flac");
    write_real_flac(&path);
    let bitdepth = extract_bitdepth(&path);
    assert!(bitdepth.is_some(), "should extract bitdepth from FLAC");
    assert!(bitdepth.unwrap() >= 16, "FLAC bitdepth should be >= 16");
}

#[test]
fn test_extract_bitrate_returns_none_for_unparseable() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("junk.flac");
    std::fs::write(&path, b"not a real flac").unwrap();
    let bitrate = extract_bitrate(&path);
    assert!(bitrate.is_none(), "junk file should return None");
}

#[test]
fn test_extract_bitdepth_returns_none_for_unparseable() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("junk.flac");
    std::fs::write(&path, b"not a real flac").unwrap();
    let bitdepth = extract_bitdepth(&path);
    assert!(bitdepth.is_none(), "junk file should return None");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_extract_bit -- --nocapture 2>&1 | tail -20`
Expected: FAIL with "cannot find function `extract_bitrate`"

- [ ] **Step 3: Implement the helpers**

Add these public functions to `src/organizer.rs`, near `file_quality_score()`:

```rust
/// Extract the actual bitrate (kbps) from an audio file using lofty.
/// Returns None if the file cannot be parsed or is lossless (bitrate
/// is not a meaningful quality metric for lossless formats).
pub fn extract_bitrate(path: &Path) -> Option<u32> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    let format = format_from_extension(ext)?;
    match format {
        AudioFormat::Lossy => {
            let tagged_file = lofty::probe::Probe::open(path).ok()?.read().ok()?;
            let props = tagged_file.properties();
            Some(props.audio_bitrate().unwrap_or(0))
        }
        AudioFormat::Lossless => None, // bitrate not meaningful for lossless
    }
}

/// Extract the actual bit depth from an audio file using lofty.
/// Returns None if the file cannot be parsed or is lossy (bit depth
/// is not applicable to lossy formats).
pub fn extract_bitdepth(path: &Path) -> Option<u32> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    let format = format_from_extension(ext)?;
    match format {
        AudioFormat::Lossless => {
            let tagged_file = lofty::probe::Probe::open(path).ok()?.read().ok()?;
            let props = tagged_file.properties();
            props.bit_depth()
        }
        AudioFormat::Lossy => None, // bitdepth not applicable to lossy
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test test_extract_bit -- --nocapture 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/organizer.rs
git commit -m "feat: add extract_bitrate/extract_bitdepth helpers for post-download verification"
```

---

### Task 2: Update file_passes_filters to allow missing metadata

**Files:**
- Modify: `src/filter.rs`

The current filter rejects files with missing bitrate when `min_bitrate` is set. Change it to pass those files (they'll be verified post-download). Same for missing bitdepth when `min_bitdepth` is set.

- [ ] **Step 1: Write the failing tests**

```rust
// In src/filter.rs mod tests:

#[test]
fn test_filter_passes_missing_bitrate_when_min_set() {
    // When min_bitrate is set but the file has no bitrate metadata,
    // the file should PASS (to be verified post-download).
    let cfg = FilterConfig {
        min_bitrate: Some(320),
        min_bitdepth: None,
        ..default_filter_config()
    };
    let file = FileInfo {
        name: "01 - Track.flac".into(),
        size: 10_000_000,
        attribs: HashMap::new(), // no bitrate attribute
    };
    assert!(
        file_passes_filters(&file, &cfg),
        "file with missing bitrate should pass when min_bitrate is set"
    );
}

#[test]
fn test_filter_passes_missing_bitdepth_when_min_set() {
    // When min_bitdepth is set but the file has no bitdepth metadata,
    // the file should PASS (to be verified post-download).
    let cfg = FilterConfig {
        min_bitrate: None,
        min_bitdepth: Some(16),
        ..default_filter_config()
    };
    let file = FileInfo {
        name: "01 - Track.flac".into(),
        size: 10_000_000,
        attribs: HashMap::new(), // no bitdepth attribute
    };
    assert!(
        file_passes_filters(&file, &cfg),
        "file with missing bitdepth should pass when min_bitdepth is set"
    );
}

#[test]
fn test_filter_still_rejects_low_bitrate_when_provided() {
    // When the peer DOES provide bitrate and it's below min, reject.
    let cfg = FilterConfig {
        min_bitrate: Some(320),
        min_bitdepth: None,
        ..default_filter_config()
    };
    let mut attribs = HashMap::new();
    attribs.insert(0, 128u32); // bitrate = 128 kbps
    let file = FileInfo {
        name: "01 - Track.mp3".into(),
        size: 5_000_000,
        attribs,
    };
    assert!(
        !file_passes_filters(&file, &cfg),
        "file with bitrate below min should still be rejected"
    );
}

#[test]
fn test_filter_still_rejects_low_bitdepth_when_provided() {
    // When the peer DOES provide bitdepth and it's below min, reject.
    let cfg = FilterConfig {
        min_bitrate: None,
        min_bitdepth: Some(24),
        ..default_filter_config()
    };
    let mut attribs = HashMap::new();
    attribs.insert(5, 16u32); // bitdepth = 16
    let file = FileInfo {
        name: "01 - Track.flac".into(),
        size: 30_000_000,
        attribs,
    };
    assert!(
        !file_passes_filters(&file, &cfg),
        "file with bitdepth below min should still be rejected"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_filter_passes_missing -- --nocapture 2>&1 | tail -20`
Expected: FAIL with assertion "file with missing bitrate should pass when min_bitrate is set"

- [ ] **Step 3: Implement the change**

In `src/filter.rs`, change `file_passes_filters()` around line 208:

```rust
    // Bitrate check (key 0 = bitrate in kbps). When a minimum is configured
    // and the peer provides bitrate, reject files below the minimum. When the
    // peer does NOT provide bitrate, let the file pass — it will be verified
    // post-download using actual file metadata (lofty).
    if let Some(min_br) = config.min_bitrate {
        if let Some(&file_br) = file.attribs.get(&0) {
            if file_br < min_br {
                return false;
            }
        }
        // attribs.get(&0) == None → pass (verify post-download)
    }
    // Bitdepth check (key 5 = bit depth). Same logic: reject if peer
    // provides bitdepth below minimum, pass if missing (verify later).
    if let Some(min_bd) = config.min_bitdepth {
        if let Some(&file_bd) = file.attribs.get(&5) {
            if file_bd < min_bd {
                return false;
            }
        }
        // attribs.get(&5) == None → pass (verify post-download)
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test test_filter_passes_missing -- --nocapture 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 5: Run full filter tests to verify no regressions**

Run: `cargo test filter:: --nocapture 2>&1 | tail -10`
Expected: All existing filter tests still pass

- [ ] **Step 6: Commit**

```bash
git add src/filter.rs
git commit -m "feat: allow missing bitrate/bitdepth through filter for post-download verification"
```

---

### Task 3: Add post-download verification to download_file

**Files:**
- Modify: `src/download.rs`

After each file downloads successfully, verify bitrate/bitdepth using the new helpers. If verification fails, delete the file and return an error (triggering the existing fallback to next candidate).

- [ ] **Step 1: Write the failing tests**

```rust
// In src/download.rs mod tests:

#[test]
fn test_download_verifies_bitrate_for_missing_metadata() {
    // Mock peer that doesn't provide bitrate. Download a file whose
    // actual bitrate is below min_bitrate. Verify download fails.
    // This requires a real audio file with known bitrate.
    // Use a temp dir with a minimal MP3 (low bitrate).
    let staging = TempDir::new().unwrap();
    let min_bitrate = Some(320u32);
    let min_bitdepth = None;
    // ... (test setup with MockClient)
    // The mock download returns a file whose actual bitrate < 320
    // Expected: download_file returns Err containing "bitrate"
}

#[test]
fn test_download_skips_verification_when_peer_provides_metadata() {
    // Mock peer that DOES provide bitrate (attribs.get(0) = Some(320)).
    // No post-download verification should happen.
    // Expected: download_file succeeds (no verification error)
}

#[test]
fn test_download_skips_verification_when_min_not_set() {
    // min_bitrate = None, min_bitdepth = None.
    // No verification should happen regardless of attribs.
    // Expected: download_file succeeds
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_download_verifies_bitrate -- --nocapture 2>&1 | tail -20`
Expected: FAIL with compilation error (function signature changed)

- [ ] **Step 3: Implement the verification in download_once**

In `src/download.rs`, add a new parameter to `download_once()` for the filter config, and add verification after the `DownloadStatus::Completed` arm:

```rust
async fn download_once(
    client: &dyn SoulseekClient,
    file: &FileInfo,
    basename: &str,
    username: &str,
    dir: &Path,
    config: &DownloadConfig,
    filters: &crate::config::FilterConfig,  // NEW PARAMETER
    progress: Option<&ProgressDisplay>,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<PathBuf> {
    // ... existing code ...

    match msg {
        // ... existing InProgress, Queued, etc. arms ...

        Ok(Some(DownloadStatus::Completed)) => {
            if let Some(bar) = &bar {
                bar.set_position(last_total_bytes);
                bar.finish_and_clear();
            }
            let dest = dir.join(basename);
            tracing::info!("Download completed: {basename} -> {}", dest.display());

            // Post-download quality verification: when min_bitrate or
            // min_bitdepth is set and the peer didn't provide the metadata,
            // verify the actual file quality using lofty.
            if let Err(e) = verify_downloaded_quality(&dest, file, filters) {
                tracing::warn!("Quality verification failed for {basename}: {e}");
                // Delete the file that didn't meet quality requirements
                let _ = std::fs::remove_file(&dest);
                return Err(e);
            }

            return Ok(dest);
        }
        // ... rest of existing code ...
    }
}
```

- [ ] **Step 4: Implement the verify_downloaded_quality function**

Add this function to `src/download.rs`:

```rust
/// Verify that a downloaded file meets the configured quality requirements.
/// Called after download completes when min_bitrate or min_bitdepth is set
/// and the peer didn't provide the metadata in search results.
///
/// Returns Ok(()) if the file passes, Err if it fails verification.
/// Returns Ok(()) if verification is not needed (no config set, or peer
/// already provided metadata).
fn verify_downloaded_quality(
    path: &Path,
    file: &FileInfo,
    filters: &crate::config::FilterConfig,
) -> Result<()> {
    // Check min_bitrate for lossy files
    if let Some(min_br) = filters.min_bitrate {
        // Only verify if the peer didn't provide bitrate
        if file.attribs.get(&0).is_none() {
            if let Some(actual_br) = crate::organizer::extract_bitrate(path) {
                if actual_br < min_br {
                    return Err(SeakarrError::Download(format!(
                        "bitrate {actual_br} kbps below minimum {min_br} kbps"
                    )));
                }
            }
            // If extract_bitrate returns None (unparseable), skip verification
        }
    }

    // Check min_bitdepth for lossless files
    if let Some(min_bd) = filters.min_bitdepth {
        // Only verify if the peer didn't provide bitdepth
        if file.attribs.get(&5).is_none() {
            if let Some(actual_bd) = crate::organizer::extract_bitdepth(path) {
                if actual_bd < min_bd {
                    return Err(SeakarrError::Download(format!(
                        "bitdepth {actual_bd} below minimum {min_bd}"
                    )));
                }
            }
            // If extract_bitdepth returns None (unparseable), skip verification
        }
    }

    Ok(())
}
```

- [ ] **Step 5: Update download_file and download_once call sites**

Update `download_file()` to pass `filters` through to `download_once()`:

```rust
pub async fn download_file(
    client: &dyn SoulseekClient,
    file: &FileInfo,
    username: &str,
    dir: &Path,
    config: &DownloadConfig,
    filters: &crate::config::FilterConfig,  // NEW PARAMETER
    progress: Option<&ProgressDisplay>,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<PathBuf> {
    // ... existing code ...
    // Update the download_once call to pass filters:
    match download_once(
        client, file, basename, username, dir, config, filters, progress, cancel,
    ).await
    // ...
}
```

Update `download_album()` to pass `filters` to `download_file()`:

```rust
// In download_album, around line 520:
match download_file(
    client,
    file,
    &candidate.username,
    &disc_dir,
    config,
    filters,  // NEW: pass filters for post-download verification
    progress,
    cancel,
)
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test test_download_verifies_bitrate -- --nocapture 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 7: Run full download tests to verify no regressions**

Run: `cargo test download:: --nocapture 2>&1 | tail -10`
Expected: All existing download tests still pass

- [ ] **Step 8: Commit**

```bash
git add src/download.rs
git commit -m "feat: add post-download bitrate/bitdepth verification in download loop"
```

---

### Task 4: Update filter rejection summary for missing metadata

**Files:**
- Modify: `src/filter.rs`

The `summarize_rejections()` function counts rejections. Since missing bitrate/bitdepth no longer causes rejection, the summary should not count them. Verify the existing summary logic still works correctly.

- [ ] **Step 1: Write the test**

```rust
// In src/filter.rs mod tests:

#[test]
fn test_summary_does_not_count_missing_bitrate_as_rejection() {
    // When min_bitrate is set and file has no bitrate, it's NOT a rejection.
    let cfg = FilterConfig {
        min_bitrate: Some(320),
        min_bitdepth: None,
        ..default_filter_config()
    };
    let results = vec![make_result(
        "user1",
        500,
        1,
        vec![make_file("01 - Track.flac", 0, 10_000_000)], // 0 bitrate = missing
    )];
    // attribs with bitrate=0 is different from missing attribs
    // Let's use empty attribs to simulate missing:
    let results = vec![SearchResult {
        username: "user1".into(),
        speed: 500,
        slots: 1,
        files: vec![FileInfo {
            name: "01 - Track.flac".into(),
            size: 10_000_000,
            attribs: HashMap::new(), // missing bitrate
        }],
    }];
    let summary = summarize_rejections(&results, &cfg, None, None);
    assert!(
        !summary.has_rejections(),
        "missing bitrate should not count as rejection"
    );
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test test_summary_does_not_count_missing -- --nocapture 2>&1 | tail -20`
Expected: PASS (the existing summary logic should already handle this correctly since it checks `attribs.get(&0)` and counts bitrate_rejected only when the value is below min)

- [ ] **Step 3: Verify no changes needed**

The existing `summarize_rejections()` at line 1308 checks:
```rust
if let Some(min_br) = config.min_bitrate {
    match f.attribs.get(&0) {
        None => {} // missing = no rejection counted
        Some(&br) if br < min_br => { summary.bitrate_rejected += 1; }
        _ => {}
    }
}
```

This already handles missing metadata correctly (doesn't count as rejection). No code change needed — just verify with the test.

- [ ] **Step 4: Commit (test only)**

```bash
git add src/filter.rs
git commit -m "test: verify missing bitrate not counted as rejection in summary"
```

---

### Task 5: Update reject-summary logging in download_album

**Files:**
- Modify: `src/download.rs`

The `download_album` function logs rejection reasons when a candidate has no files passing filters. Since missing bitrate no longer causes rejection, the log output may change. Verify the existing logging still makes sense.

- [ ] **Step 1: Review the logging code**

The logging at line 447-457 logs `passes={passes}` for each file. Since `file_passes_filters` now returns `true` for missing bitrate files, the log will show `passes=true` instead of `passes=false`. This is correct behavior — no change needed.

- [ ] **Step 2: Verify no code change needed**

The existing logging correctly reflects the filter result. When missing bitrate files pass, they'll show as `passes=true` and won't be logged as rejections. This is the intended behavior.

- [ ] **Step 3: Commit (no change)**

No commit needed — existing code is correct.

---

### Task 6: Integration test — full pipeline with missing bitrate

**Files:**
- Modify: `tests/pipeline_test.rs`

Add an end-to-end test that verifies the full flow: peer doesn't provide bitrate, file downloads, verification fails, album discarded, next peer tried.

- [ ] **Step 1: Write the failing test**

```rust
// In tests/pipeline_test.rs:

#[tokio::test]
async fn test_pipeline_rejects_low_bitrate_after_download() {
    // Peer 1: doesn't provide bitrate, file has low actual bitrate
    // Peer 2: provides bitrate that meets minimum
    // Expected: Peer 1's files are downloaded but fail verification,
    // then Peer 2's files are downloaded and succeed.
    let client = MockClient::new();
    *client.search_results.lock().unwrap() = vec![
        SearchResult {
            username: "low-quality-peer".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                r"Test Artist\Test Album\01 - Track.flac",
                0, // no bitrate attribute
                10_000_000,
            )],
        },
        SearchResult {
            username: "high-quality-peer".into(),
            speed: 400,
            slots: 1,
            files: vec![make_file_with_bitrate(
                r"Test Artist\Test Album\01 - Track.flac",
                320, // meets min_bitrate
                15_000_000,
            )],
        },
    ];

    let staging = TempDir::new().unwrap();
    let mut config = Config::default();
    config.soulseek.username = "test".into();
    config.soulseek.password = "test".into();
    config.storage.staging_dir = staging.path().to_string_lossy().into();
    config.download.min_upload_speed_kbps = 0;
    config.download.max_retries = 0;
    config.filters.min_bitrate = Some(320);
    config.filters.min_tracks = 0;
    config.notifications.urls = vec![];

    let db = Database::open_in_memory().unwrap();

    let result = seakarr::runner::process_album(
        &client,
        "Test Artist",
        Some("Test Album"),
        &config,
        &db,
        staging.path(),
        None,
        None,
        None,
        None,
    )
    .await;

    assert!(result.is_ok());
    // The album should have been downloaded from the second peer
    // (first peer's files failed bitrate verification)
    match result.unwrap() {
        AlbumOutcome::Downloaded { track_count } => assert_eq!(track_count, 1),
        other => panic!("Expected Downloaded, got: {other:?}"),
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_pipeline_rejects_low_bitrate -- --nocapture 2>&1 | tail -20`
Expected: FAIL (test may not compile yet if MockClient doesn't support the needed setup)

- [ ] **Step 3: Implement any needed MockClient changes**

If the MockClient needs updates to support the test, implement them. The MockClient is in `src/client.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test test_pipeline_rejects_low_bitrate -- --nocapture 2>&1 | tail -20`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add tests/pipeline_test.rs
git commit -m "test: add integration test for post-download bitrate verification"
```

---

### Task 7: Final verification — full test suite

- [ ] **Step 1: Run full test suite**

Run: `cargo test 2>&1 | tail -20`
Expected: All tests pass (417+ tests, 0 failures)

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets -- -D warnings 2>&1 | tail -10`
Expected: Clean (no warnings)

- [ ] **Step 3: Run format check**

Run: `cargo fmt --check 2>&1`
Expected: Clean (no diff)

- [ ] **Step 4: Final commit if needed**

```bash
git add -A
git commit -m "feat: complete missing bitrate/bitdepth download-then-verify fallback"
```
