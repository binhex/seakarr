# Library Track Count Check — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `peer_track_count` filter that rejects search results where the peer has fewer usable tracks than the library already has — preventing silent downgrades during auto-mode album upgrades.

**Architecture:** Pass the library track count from the scanner through `run_auto_mode` → `process_album` → `filter_results` as `Option<usize>`. The filter check sits after the existing `min_tracks` and `contiguity` gates. `None` (batch/manual mode) skips the check automatically.

**Tech Stack:** Rust, tokio, serde, tracing

---

## File Structure

| File | Change |
|---|---|
| `src/config.rs` | Add `peer_track_count: bool` to `FilterConfig` with serde default `true`; update `Config::default()` |
| `src/scanner.rs` | `find_albums_to_upgrade` returns `Vec<(String, String, usize)>` (add track_count); update2 existing tests |
| `src/runner.rs` | `process_album` takes `library_track_count: Option<usize>`; `run_auto_mode` unpacks the triple; update all test call sites |
| `src/filter.rs` | `filter_results` takes `library_track_count: Option<usize>`; adds the check after min_tracks/contiguity;5 new tests |
| `tests/pipeline_test.rs` | Update2 `process_album` call sites (pass `None`) |

---

### Task 1: Add `peer_track_count` to `FilterConfig`

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Add the field to `FilterConfig`**

In `src/config.rs`, add after the `min_tracks` field (line ~106):

```rust
    #[serde(default = "default_true")]
    pub peer_track_count: bool,
```

- [ ] **Step 2: Add the field to `Config::default()`**

In `src/config.rs`, in the `filters: FilterConfig { ... }` block inside the `Default for Config` impl (around line ~498), add after `min_tracks: default_min_tracks(),`:

```rust
                peer_track_count: default_true(),
```

- [ ] **Step 3: Update all `FilterConfig` literal constructions**

Add `peer_track_count: true,` (or `peer_track_count: default_true(),` for non-test code) to every `FilterConfig { ... }` literal in the codebase. There are ~20 locations:

- `src/config.rs:491` — `Config::default()` (done in Step 2)
- `src/scanner.rs:234` — `test_find_albums_to_upgrade_below_bitrate`
- `src/scanner.rs:264` — `test_find_albums_to_upgrade_wrong_format`
- `src/download.rs:379` — `default_filter_config_test()`
- `src/filter.rs:170` — `default_filter_config()`
- `src/filter.rs:209, 289, 315, 335, 358, 386, 409, 437, 458, 478, 503` — individual filter tests

Use `peer_track_count: true,` in test literals and `peer_track_count: default_true(),` in production default code.

- [ ] **Step 4: Verify compilation**

Run: `cargo build 2>&1 | tail -5`
Expected: `Finished dev profile`

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/scanner.rs src/download.rs src/filter.rs
git commit -m "feat: add peer_track_count field to FilterConfig"
```

---

### Task 2: Update `find_albums_to_upgrade` return type

**Files:**
- Modify: `src/scanner.rs`

- [ ] **Step 1: Change the return type**

In `src/scanner.rs`, change the function signature (line ~134):

From: `pub fn find_albums_to_upgrade(albums: &[ScannedAlbum], config: &FilterConfig) -> Vec<(String, String)>`
To: `pub fn find_albums_to_upgrade(albums: &[ScannedAlbum], config: &FilterConfig) -> Vec<(String, String, usize)>`

- [ ] **Step 2: Update the return value**

In the same function, find the `.map(|a| (a.artist.clone(), a.album.clone()))` at the end and change it to:

`.map(|a| (a.artist.clone(), a.album.clone(), a.track_count))`

- [ ] **Step 3: Update the2 existing scanner tests**

In `test_find_albums_to_upgrade_below_bitrate` (line ~212), change the assertions:

```rust
        let to_upgrade = find_albums_to_upgrade(&albums, &config);
        assert_eq!(to_upgrade.len(), 1);
        assert_eq!(to_upgrade[0].0, "Artist1");
        assert_eq!(to_upgrade[0].1, "Album1");
        assert_eq!(to_upgrade[0].2, 3); // track_count
```

In `test_find_albums_to_upgrade_wrong_format` (line ~253), change the assertion:

```rust
        let to_upgrade = find_albums_to_upgrade(&albums, &config);
        assert_eq!(to_upgrade.len(), 1);
        assert_eq!(to_upgrade[0].0, "Artist");
        assert_eq!(to_upgrade[0].2, 2); // track_count
```

- [ ] **Step 4: Update the caller in `run_auto_mode`**

In `src/runner.rs`, line ~339, change:

From: `let targets = scanner::find_albums_to_upgrade(&albums, &config.filters);`
To: `let targets_with_counts = scanner::find_albums_to_upgrade(&albums, &config.filters);`

And change the loop that follows (line ~345) from:

```rust
let targets_vec: Vec<(String, String)> = targets;
```

To:

```rust
let targets_vec: Vec<(String, String, usize)> = targets_with_counts;
```

And update the loop to unpack the triple (line ~350):

```rust
for (artist, album, track_count) in &targets_vec {
    let semaphore = Arc::clone(&semaphore);
    let progress = progress.clone();
    let cancel = cancel.clone();
    let artist = artist.clone();
    let album = album.clone();
    let library_track_count = *track_count;
    // ...
    let result = process_album(
        client,
        &artist,
        Some(&album),
        config,
        db,
        staging_dir,
        progress.as_deref(),
        Some(&cancel),
        Some(library_track_count),
    )
    .await;
    (artist, album, result)
}
```

- [ ] **Step 5: Verify compilation**

Run: `cargo build 2>&1 | tail -5`
Expected: compile errors from process_album and filter_results signature mismatches (fixed in Task 3)

---

### Task 3: Thread `library_track_count` through `process_album` → `filter_results`

**Files:**
- Modify: `src/runner.rs` (process_album signature)
- Modify: `src/filter.rs` (filter_results signature)
- Modify: `tests/pipeline_test.rs` (call sites)
- Modify: `src/runner.rs` (test call sites)

- [ ] **Step 1: Update `process_album` signature**

In `src/runner.rs`, add the new parameter after `cancel`:

```rust
pub async fn process_album(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    config: &Config,
    db: &Database,
    staging_dir: &Path,
    progress: Option<&ProgressDisplay>,
    cancel: Option<&Arc<AtomicBool>>,
    library_track_count: Option<usize>,
) -> Result<AlbumOutcome> {
```

- [ ] **Step 2: Pass `library_track_count` to `filter_results`**

In `src/runner.rs`, find the call to `filter::filter_results` (appears twice — primary search and fallback). Change both from:

`let filtered = filter::filter_results(&results, &config.filters);`

To:

`let filtered = filter::filter_results(&results, &config.filters, library_track_count);`

And for the fallback:

`filtered = filter::filter_results(&fallback_results, &config.filters, library_track_count);`

- [ ] **Step 3: Update `filter_results` signature**

In `src/filter.rs`, change the signature (line ~9):

From: `pub fn filter_results(results: &[SearchResult], config: &FilterConfig) -> Vec<SearchResult> {`
To: `pub fn filter_results(results: &[SearchResult], config: &FilterConfig, library_track_count: Option<usize>) -> Vec<SearchResult> {`

No logic change yet — just threading the parameter through.

- [ ] **Step 4: Update all `filter_results` call sites in tests**

In `src/filter.rs` tests, every call to `filter_results(...)` needs the third argument. For existing tests that don't test the library track count check, pass `None`:

Change: `filter_results(&results, &cfg)`
To: `filter_results(&results, &cfg, None)`

There are ~15 call sites in the filter.rs test module. Update each one.

- [ ] **Step 5: Update `process_album` call sites in runner.rs tests**

In `src/runner.rs` test module, every call to `process_album(...)` needs the new parameter. Pass `None` for all existing tests:

Change: add `None,` as the last argument (after `cancel`).

There are ~10 call sites in the runner.rs test module. Update each one.

- [ ] **Step 6: Update `process_album` call sites in pipeline_test.rs**

In `tests/pipeline_test.rs`, both calls to `seakarr::runner::process_album(...)` need the new parameter. Pass `None`:

```rust
    let result = seakarr::runner::process_album(
        &client,
        "Test Artist",
        Some("Test Album"),
        &config,
        &db,
        staging.path(),
        None,
        None,
        None,  // library_track_count (not applicable in manual mode)
    )
    .await;
```

- [ ] **Step 7: Verify all tests pass**

Run: `cargo test 2>&1 | grep "test result"`
Expected: all pass (no logic change yet, just signature threading)

- [ ] **Step 8: Commit**

```bash
git add src/runner.rs src/filter.rs tests/pipeline_test.rs
git commit -m "refactor: thread library_track_count through process_album and filter_results"
```

---

### Task 4: Add the filter check (TDD)

**Files:**
- Modify: `src/filter.rs`

- [ ] **Step 1: Write the5 failing tests**

Add these tests to the `#[cfg(test)] mod tests` block in `src/filter.rs`:

```rust
    #[test]
    fn test_peer_track_count_rejects_lesser() {
        // Peer has 3 filtered files, library has 5 → rejected.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bitrate: None,
            min_bitdepth: None,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: false,
            min_tracks: 1,
            peer_track_count: true,
        };
        let results = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file("01 - track.flac", 900, 10_000_000),
                make_file("02 - track.flac", 900, 10_000_000),
                make_file("03 - track.flac", 900, 10_000_000),
            ],
        }];
        let filtered = filter_results(&results, &cfg, Some(5));
        assert!(filtered.is_empty(), "3 tracks < library 5 → rejected");
    }

    #[test]
    fn test_peer_track_count_accepts_equal() {
        // Peer has 5 filtered files, library has 5 → passes.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bitrate: None,
            min_bitdepth: None,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: false,
            min_tracks: 1,
            peer_track_count: true,
        };
        let results = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file("01 - track.flac", 900, 10_000_000),
                make_file("02 - track.flac", 900, 10_000_000),
                make_file("03 - track.flac", 900, 10_000_000),
                make_file("04 - track.flac", 900, 10_000_000),
                make_file("05 - track.flac", 900, 10_000_000),
            ],
        }];
        let filtered = filter_results(&results, &cfg, Some(5));
        assert_eq!(filtered.len(), 1, "5 tracks == library 5 → accepted");
    }

    #[test]
    fn test_peer_track_count_accepts_greater() {
        // Peer has 7 filtered files, library has 5 → passes.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bitrate: None,
            min_bitdepth: None,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: false,
            min_tracks: 1,
            peer_track_count: true,
        };
        let results = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file("01 - track.flac", 900, 10_000_000),
                make_file("02 - track.flac", 900, 10_000_000),
                make_file("03 - track.flac", 900, 10_000_000),
                make_file("04 - track.flac", 900, 10_000_000),
                make_file("05 - track.flac", 900, 10_000_000),
                make_file("06 - track.flac", 900, 10_000_000),
                make_file("07 - track.flac", 900, 10_000_000),
            ],
        }];
        let filtered = filter_results(&results, &cfg, Some(5));
        assert_eq!(filtered.len(), 1, "7 tracks > library 5 → accepted");
    }

    #[test]
    fn test_peer_track_count_disabled() {
        // peer_track_count: false → peer with 3 files passes even though
        // library has 5.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bitrate: None,
            min_bitdepth: None,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: false,
            min_tracks: 1,
            peer_track_count: false,
        };
        let results = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file("01 - track.flac", 900, 10_000_000),
                make_file("02 - track.flac", 900, 10_000_000),
                make_file("03 - track.flac", 900, 10_000_000),
            ],
        }];
        let filtered = filter_results(&results, &cfg, Some(5));
        assert_eq!(filtered.len(), 1, "check disabled → accepted regardless");
    }

    #[test]
    fn test_peer_track_count_none_skips() {
        // library_track_count: None (batch/manual mode) → check skipped.
        let cfg = FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bitrate: None,
            min_bitdepth: None,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: false,
            min_tracks: 1,
            peer_track_count: true,
        };
        let results = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![
                make_file("01 - track.flac", 900, 10_000_000),
                make_file("02 - track.flac", 900, 10_000_000),
                make_file("03 - track.flac", 900, 10_000_000),
            ],
        }];
        let filtered = filter_results(&results, &cfg, None);
        assert_eq!(filtered.len(), 1, "library_track_count None → check skipped");
    }
```

Note: these tests use `make_file` which is already defined in the filter.rs test module. If it doesn't exist, check the existing test helpers — the filter tests use a similar helper pattern.

- [ ] **Step 2: Run tests to verify RED**

Run: `cargo test -p seakarr --lib filter::tests::test_peer_track_count 2>&1 | tail -10`
Expected: tests fail with "not yet implemented" or compilation error (the check logic doesn't exist yet)

- [ ] **Step 3: Implement the filter check**

In `src/filter.rs`, in the `filter_results` function, add the check in **both branches** (contiguous_tracks ON and OFF).

**In the `!config.contiguous_tracks` branch** — restructure the return:

From:
```rust
                let passing_count = r.files.iter().filter(|f| safe_and_passing(f)).count();
                let min = config.min_tracks.max(1) as usize;
                return passing_count >= min;
```

To:
```rust
                let passing_count = r.files.iter().filter(|f| safe_and_passing(f)).count();
                let min = config.min_tracks.max(1) as usize;
                if passing_count < min {
                    return false;
                }
                // Library track count check (auto mode only).
                if let Some(lib_count) = library_track_count {
                    if config.peer_track_count && passing_count < lib_count {
                        tracing::debug!(
                            "result from {} rejected: {} filtered tracks < library track count {}",
                            r.username,
                            passing_count,
                            lib_count
                        );
                        return false;
                    }
                }
                return true;
```

**In the contiguous_tracks ON branch** — add after the contiguity check:

After:
```rust
            if !crate::tracks::files_have_contiguous_tracks(&passing) {
                // ... existing rejection ...
                return false;
            }
```

Add:
```rust
            // Library track count check (auto mode only).
            if let Some(lib_count) = library_track_count {
                if config.peer_track_count && passing.len() < lib_count {
                    tracing::debug!(
                        "result from {} rejected: {} filtered tracks < library track count {}",
                        r.username,
                        passing.len(),
                        lib_count
                    );
                    return false;
                }
            }
```

- [ ] **Step 4: Run tests to verify GREEN**

Run: `cargo test -p seakarr --lib filter::tests::test_peer_track_count 2>&1 | tail -10`
Expected: all5 tests PASS

- [ ] **Step 5: Run the full filter test suite**

Run: `cargo test -p seakarr --lib filter::tests 2>&1 | tail -5`
Expected: all tests PASS (existing tests + new tests)

- [ ] **Step 6: Commit**

```bash
git add src/filter.rs
git commit -m "feat: add library track count check in filter_results"
```

---

### Task 5: Full verification

- [ ] **Step 1: Run the full test suite**

Run: `cargo test 2>&1 | grep "test result"`
Expected: all tests pass across all crates

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets --all-features --workspace -- -D warnings 2>&1 | tail -5`
Expected: `Finished dev profile` — zero warnings

- [ ] **Step 3: Run fmt check**

Run: `cargo fmt --check 2>&1`
Expected: clean (no diff)

- [ ] **Step 4: Commit any formatting fixes**

If `cargo fmt` made changes:
```bash
cargo fmt
git add -A
git commit -m "style: cargo fmt"
```

---

## Non-Goals (from spec)

- No changes to daemon mode behavior (uses run_auto_mode, gets the check automatically)
- No changes to search or download logic — filter-stage only
- No DB schema changes — library track count is transient
- No config migration — serde default `true` handles existing configs
