# Search Fallback Overhaul Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an album-only fallback tier and filter out generic track names to prevent wrong-album downloads.

**Architecture:** `search_album_with_fallback` gains a new album-only tier (search by album name only, verify artist via `path_matches_artist`). `search_by_title` gains generic name filtering (`CD Track N`, `Track N`, etc.) to prevent garbage queries. Runner filters generic filenames before title-search fallback.

**Tech Stack:** Rust, `regex` (already in Cargo.toml), `unicode-normalization` (already used)

---

## File Structure

| File | Responsibility | Changes |
|------|---------------|---------|
| `src/search.rs` | Search logic, fallback tiers, track name utilities | Move `path_matches_artist` to production; add `is_generic_track_name`; modify `search_album_with_fallback`; modify `search_by_title` |
| `src/runner.rs` | Orchestration, fallback call sites | Filter generic filenames before title-search fallback; add logging |

---

### Task 1: Move `path_matches_artist` to Production Code

**Files:**
- Modify: `src/search.rs:359-407` (remove `#[cfg(test)]` attributes)

**Context:** `path_matches_artist` is currently `#[cfg(test)]` only — it was left test-only after the album-only fallback was removed. The new album-only tier needs it in production. The function itself is unchanged; only the `#[cfg(test)]` attributes are removed.

- [ ] **Step 1: Verify existing tests pass**

The function already has tests (`test_path_matches_artist_*`). Run them to confirm they pass before making changes:

Run: `cargo test --lib search::tests::test_path_matches_artist`
Expected: PASS (all existing path_matches_artist tests pass)

- [ ] **Step 3: Remove `#[cfg(test)]` attributes**

In `src/search.rs`, remove the `#[cfg(test)]` attribute from:
1. Line 359: `#[cfg(test)]` before `const ARTIST_STOP_WORDS` — remove this line
2. Line 383: `#[cfg(test)]` before `pub fn path_matches_artist` — remove this line

The function becomes production-accessible. The `ARTIST_STOP_WORDS` constant also becomes production-accessible (needed by the function).

- [ ] **Step 4: Run all tests to verify nothing breaks**

Run: `cargo test`
Expected: All tests pass (removing `#[cfg(test)]` doesn't break existing tests)

- [ ] **Step 5: Commit**

```bash
git add src/search.rs
git commit -m "refactor: move path_matches_artist from test-only to production code"
```

---

### Task 2: Add `is_generic_track_name` Function

**Files:**
- Modify: `src/search.rs` (add new function after `clean_track_title`)

**Context:** Generic track names like "CD Track 1", "Track 1", "Untitled" produce garbage search queries. This function identifies them so they can be filtered out before building search queries.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/search.rs`:

```rust
#[test]
fn test_is_generic_track_name_cd_track() {
    assert!(is_generic_track_name("CD Track 1.mp3"));
    assert!(is_generic_track_name("CD Track 01.flac"));
    assert!(is_generic_track_name("CD Track1.mp3"));
    assert!(is_generic_track_name("cd track 5.mp3"));
}

#[test]
fn test_is_generic_track_name_track() {
    assert!(is_generic_track_name("Track 1.mp3"));
    assert!(is_generic_track_name("Track 01.flac"));
    assert!(is_generic_track_name("Track1.mp3"));
    assert!(is_generic_track_name("track 5.mp3"));
}

#[test]
fn test_is_generic_track_name_other_patterns() {
    assert!(is_generic_track_name("Untitled.mp3"));
    assert!(is_generic_track_name("Unknown.flac"));
    assert!(is_generic_track_name("Audio 1.mp3"));
    assert!(is_generic_track_name("Recording 5.flac"));
    assert!(is_generic_track_name("01.mp3"));
    assert!(is_generic_track_name("42.flac"));
}

#[test]
fn test_is_generic_track_name_non_generic() {
    assert!(!is_generic_track_name("Tomorrow Comes Today.mp3"));
    assert!(!is_generic_track_name("Musicology.flac"));
    assert!(!is_generic_track_name("01 - Hello.mp3"));
    assert!(!is_generic_track_name("Cafés.flac"));
    assert!(!is_generic_track_name("I Miss You.mp3"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib search::tests::test_is_generic_track_name`
Expected: FAIL with "function not found" or similar

- [ ] **Step 3: Implement `is_generic_track_name`**

Add to `src/search.rs` after `clean_track_title` (around line 125):

```rust
/// Returns true if the track name is generic/meaningless for search purposes.
/// Generic names produce garbage queries that match unrelated albums.
pub fn is_generic_track_name(name: &str) -> bool {
    let cleaned = clean_track_title(name);
    if cleaned.is_empty() {
        return true;
    }
    // Patterns that indicate generic/meaningless track names
    static GENERIC_PATTERNS: OnceLock<Regex> = OnceLock::new();
    let re = GENERIC_PATTERNS.get_or_init(|| {
        Regex::new(r"(?i)^(cd\s*track|track|untitled|unknown|audio|recording|track\s*\d+|\d+)$")
            .expect("valid generic pattern regex")
    });
    re.is_match(&cleaned)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib search::tests::test_is_generic_track_name`
Expected: PASS

- [ ] **Step 5: Run full test suite**

Run: `cargo test`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add src/search.rs
git commit -m "feat: add is_generic_track_name to filter meaningless track names"
```

---

### Task 3: Add Album-Only Tier to `search_album_with_fallback`

**Files:**
- Modify: `src/search.rs:50-58` (`search_album_with_fallback` function)

**Context:** When the primary `"Artist Album"` search returns nothing, search by album name only and filter by artist match using `path_matches_artist`. This catches cases where the artist name is blocked but the album name alone finds results.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/search.rs`:

```rust
#[tokio::test]
async fn test_album_only_fallback_when_primary_empty() {
    let client = MockClient::new();
    // Primary "Prince Musicology" returns nothing (blocked artist)
    // Album-only "Musicology" returns results with artist in path
    client.search_results_by_query.lock().unwrap().insert(
        "Musicology".into(),
        vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                "Prince/Musicology/01 - Musicology.flac",
                900,
                10_000_000,
            )],
        }],
    );

    let outcome = search_album_with_fallback(&client, "Prince", Some("Musicology"), 15)
        .await
        .unwrap();
    assert_eq!(outcome.results.len(), 1);
    assert_eq!(outcome.results[0].username, "user1");
}

#[tokio::test]
async fn test_album_only_fallback_filters_by_artist() {
    let client = MockClient::new();
    // Album-only "Musicology" returns results with WRONG artist
    client.search_results_by_query.lock().unwrap().insert(
        "Musicology".into(),
        vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                "Some Other Artist/Musicology/01 - Track.flac",
                900,
                10_000_000,
            )],
        }],
    );

    let outcome = search_album_with_fallback(&client, "Prince", Some("Musicology"), 15)
        .await
        .unwrap();
    assert!(outcome.results.is_empty());
}

#[tokio::test]
async fn test_album_only_fallback_skips_when_album_empty() {
    let client = MockClient::new();
    // Album is None — should not attempt album-only search
    let outcome = search_album_with_fallback(&client, "Prince", None, 15)
        .await
        .unwrap();
    assert!(outcome.results.is_empty());
    // Only the primary search should have been attempted
    assert_eq!(client.search_queries.lock().unwrap().len(), 1);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib search::tests::test_album_only_fallback`
Expected: FAIL (current implementation returns empty for all)

- [ ] **Step 3: Implement album-only tier**

Replace `search_album_with_fallback` in `src/search.rs` (lines 50-58):

```rust
pub async fn search_album_with_fallback(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    timeout_secs: u64,
) -> Result<SearchOutcome> {
    // Tier 1: primary "Artist Album" search
    let results = search_album(client, artist, album, timeout_secs).await?;
    if !results.is_empty() {
        return Ok(SearchOutcome { results });
    }

    // Tier 2: album-only search (when primary returns nothing)
    if let Some(album_name) = album {
        if !album_name.trim().is_empty() {
            let album_results = search_album(client, "", Some(album_name), timeout_secs).await?;
            // Filter by artist match using existing path_matches_artist
            let artist_matches: Vec<SearchResult> = album_results
                .into_iter()
                .filter(|r| r.files.iter().any(|f| path_matches_artist(&f.name, artist)))
                .collect();
            if !artist_matches.is_empty() {
                return Ok(SearchOutcome { results: artist_matches });
            }
        }
    }

    // Both tiers returned nothing
    Ok(SearchOutcome { results: vec![] })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib search::tests::test_album_only_fallback`
Expected: PASS

- [ ] **Step 5: Run full test suite**

Run: `cargo test`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add src/search.rs
git commit -m "feat: add album-only fallback tier to search_album_with_fallback"
```

---

### Task 4: Add Generic Name Filtering to `search_by_title`

**Files:**
- Modify: `src/search.rs:270-340` (`search_by_title` function)

**Context:** When all library tracks have generic names (CD Track N, Track N, etc.), the search query is meaningless and matches unrelated albums. Filter out generic names before building the query. If all names are generic, return empty.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/search.rs`:

```rust
#[tokio::test]
async fn test_search_by_title_skips_generic_names() {
    let client = MockClient::new();
    // All tracks have generic names — should return empty without searching
    let library = vec![
        "CD Track 1.mp3".to_string(),
        "CD Track 2.mp3".to_string(),
        "CD Track 3.mp3".to_string(),
    ];
    let results = search_by_title(&client, &library, "Prince", 15, 100)
        .await
        .unwrap();
    assert!(results.is_empty());
    assert!(client.search_queries.lock().unwrap().is_empty());
}

#[tokio::test]
async fn test_search_by_title_uses_non_generic_names() {
    let client = MockClient::new();
    // Mix of generic and real names — should use only real names
    let library = vec![
        "CD Track 1.mp3".to_string(),
        "01 - Musicology.mp3".to_string(),
        "Track 3.mp3".to_string(),
    ];
    client.search_results_by_query.lock().unwrap().insert(
        "musicology".into(),
        vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file("Prince/Musicology/01 - Musicology.flac", 900, 10_000_000)],
        }],
    );

    let results = search_by_title(&client, &library, "Prince", 15, 100)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    // Verify the query was "musicology" (from the real track), not "track 1" (from generic)
    let queries = client.search_queries.lock().unwrap();
    assert!(queries.contains(&"musicology".to_string()));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib search::tests::test_search_by_title_skips_generic_names`
Expected: FAIL (current implementation doesn't filter generic names)

- [ ] **Step 3: Implement generic name filtering**

In `src/search.rs`, modify `search_by_title` (around line 280). Add filtering after the function signature, before the `clean_titles` collection:

```rust
pub async fn search_by_title(
    client: &dyn SoulseekClient,
    library_filenames: &[String],
    artist: &str,
    timeout_secs: u64,
    match_threshold_pct: u32,
) -> Result<Vec<SearchResult>> {
    // Filter out generic track names before building the query
    let non_generic: Vec<String> = library_filenames
        .iter()
        .filter(|f| !is_generic_track_name(f))
        .cloned()
        .collect();
    if non_generic.is_empty() {
        // All tracks have generic names — can't build a meaningful query
        return Ok(Vec::new());
    }
    // Clean titles — preserves order from sorted filenames for the query.
    let clean_titles: Vec<String> = non_generic
        .iter()
        .map(|filename| clean_track_title(filename))
        .filter(|t| !t.is_empty())
        .collect();
    // ... rest of function unchanged ...
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib search::tests::test_search_by_title_skips_generic_names`
Expected: PASS

- [ ] **Step 5: Run full test suite**

Run: `cargo test`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add src/search.rs
git commit -m "feat: filter generic track names from search_by_title"
```

---

### Task 5: Update Runner to Filter Generic Filenames

**Files:**
- Modify: `src/runner.rs:140-170` (title-search fallback block)

**Context:** The runner's title-search fallback should filter out generic filenames before calling `search_by_title`. If all filenames are generic, skip the fallback entirely with a log message.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/runner.rs`:

```rust
#[tokio::test]
async fn test_title_search_skips_when_all_tracks_generic() {
    let client = Arc::new(MockClient::new());
    // Primary search returns nothing
    // Library has only generic track names
    let mut config = make_test_config();
    config.search.search_title_match = 70;
    let tmp = TempDir::new().unwrap();
    let album_dir = tmp.path().join("Prince").join("The Very Best Of Prince");
    std::fs::create_dir_all(&album_dir).unwrap();
    std::fs::write(album_dir.join("CD Track 1.mp3"), b"fake").unwrap();
    std::fs::write(album_dir.join("CD Track 2.mp3"), b"fake").unwrap();
    config.library.paths = vec![tmp.path().to_string_lossy().into()];

    let db = Database::open_in_memory().unwrap();
    let staging = TempDir::new().unwrap();

    let result = process_album(
        client.as_ref() as &dyn crate::client::SoulseekClient,
        "Prince",
        Some("The Very Best Of Prince"),
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
    // Should NOT have attempted title search (all tracks generic)
    let queries = client.search_queries.lock().unwrap().clone();
    // Only the primary search should have been attempted
    assert_eq!(queries.len(), 1);
    assert_eq!(queries[0], "Prince The Very Best Of Prince");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib runner::tests::test_title_search_skips_when_all_tracks_generic`
Expected: FAIL (current implementation attempts title search with generic names)

- [ ] **Step 3: Implement generic filename filtering in runner**

In `src/runner.rs`, modify the title-search fallback block (around line 145). Replace the existing `Ok(lib_filenames) if !lib_filenames.is_empty() =>` arm with:

```rust
Ok(lib_filenames) if !lib_filenames.is_empty() => {
    title_search_attempted = true;
    let title_start = std::time::Instant::now();
    // Filter out generic track names before building the query
    let non_generic: Vec<String> = lib_filenames
        .iter()
        .filter(|f| !search::is_generic_track_name(f))
        .cloned()
        .collect();
    if non_generic.is_empty() {
        tracing::info!(
            "{artist} — {album_name}: all track names are generic, skipping title-search fallback"
        );
    } else {
        tracing::info!(
            "{artist} — {album_name}: no usable primary results, falling back to track-title search"
        );
        match search::search_by_title(
            client,
            &non_generic,
            artist,
            config.search.timeout_secs,
            config.search.search_title_match,
        )
        .await
        {
            Ok(title_results) => {
                tracing::info!(
                    "{artist} — {album_name}: title-search fallback found {} result(s)",
                    title_results.len(),
                );
                if !title_results.is_empty() {
                    total_results = title_results.iter().map(|r| r.files.len()).sum();
                    total_users = title_results.len();
                    filtered = filter::filter_results(
                        &title_results,
                        &config.filters,
                        library_track_count,
                        None,
                    );
                    last_filtered_results = title_results.clone();
                }
                let title_duration_ms = title_start.elapsed().as_millis() as u64;
                if let Err(e) = search::record_search(
                    artist,
                    Some(album_name),
                    title_results.len(),
                    title_duration_ms,
                    db,
                ) {
                    tracing::warn!(
                        "{artist} — {album_name}: failed to record title-search history: {e}"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    "{artist} — {album_name}: title-search fallback failed: {e}"
                );
            }
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib runner::tests::test_title_search_skips_when_all_tracks_generic`
Expected: PASS

- [ ] **Step 5: Run full test suite**

Run: `cargo test`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add src/runner.rs
git commit -m "feat: filter generic filenames before title-search fallback in runner"
```

---

### Task 6: Add Album-Only Fallback Integration Test

**Files:**
- Modify: `src/runner.rs` (add integration test)

**Context:** Verify the full fallback hierarchy works end-to-end: primary blocked → album-only finds match → download succeeds.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/runner.rs`:

```rust
#[tokio::test]
async fn test_album_only_fallback_fires_when_primary_empty() {
    let client = Arc::new(MockClient::new());
    // Primary "Prince Musicology" returns nothing (blocked artist)
    // Album-only "Musicology" returns results with artist in path
    client.search_results_by_query.lock().unwrap().insert(
        "Musicology".into(),
        vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                "Prince/Musicology/01 - Musicology.flac",
                900,
                10_000_000,
            )],
        }],
    );

    let mut config = make_test_config();
    let db = Database::open_in_memory().unwrap();
    let staging = TempDir::new().unwrap();

    let result = process_album(
        client.as_ref() as &dyn crate::client::SoulseekClient,
        "Prince",
        Some("Musicology"),
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
    assert_eq!(result.unwrap(), AlbumOutcome::Downloaded { track_count: 1 });

    // Verify both searches were attempted: primary + album-only
    let queries = client.search_queries.lock().unwrap().clone();
    assert_eq!(queries.len(), 2);
    assert_eq!(queries[0], "Prince Musicology");  // primary
    assert_eq!(queries[1], "Musicology");          // album-only
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib runner::tests::test_album_only_fallback_fires_when_primary_empty`
Expected: FAIL (current implementation doesn't have album-only tier in runner)

- [ ] **Step 3: Verify the test passes with the search.rs changes from Task 3**

The album-only tier is implemented in `search_album_with_fallback` (Task 3), which the runner already calls. This test verifies the integration works end-to-end.

Run: `cargo test --lib runner::tests::test_album_only_fallback_fires_when_primary_empty`
Expected: PASS

- [ ] **Step 4: Run full test suite**

Run: `cargo test`
Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add src/runner.rs
git commit -m "test: add album-only fallback integration test"
```

---

### Task 7: Final Verification and Cleanup

**Files:**
- None (verification only)

- [ ] **Step 1: Run full test suite**

Run: `cargo test`
Expected: All tests pass (should be 450+ tests)

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: No warnings

- [ ] **Step 3: Run format check**

Run: `cargo fmt --check`
Expected: No diff

- [ ] **Step 4: Run pre-commit**

Run: `pre-commit run --all-files`
Expected: All hooks pass

- [ ] **Step 5: Verify git status is clean**

Run: `git status`
Expected: Clean working tree
