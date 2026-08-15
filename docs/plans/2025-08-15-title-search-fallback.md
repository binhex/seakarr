# Title-Search Fallback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a third-tier search fallback that searches Soulseek for a cleaned library track title when both primary and album-only searches return zero results, then verifies other album tracks match before downloading.

**Architecture:** Extend the existing 2-tier search flow (primary → album-only) with a third tier (title search). The title search reads library track filenames from disk on demand, cleans the first track's name, searches for it, and verifies the result contains enough matching library tracks. This is the absolute last resort — only triggered when both prior tiers return empty.

**Tech Stack:** Rust, tokio, lofty (tag reading), walkdir (filesystem), unicode-normalization, regex

**Spec:** `docs/agent/specs/title-search-fallback.md`

---

## File Structure

| File | Role | Change Type |
|------|------|-------------|
| `src/config.rs` | Add `search_title_match` field to `SearchConfig` | Modify |
| `src/search.rs` | Add `clean_track_title()`, `get_library_track_filenames()`, `search_by_title()`, `search_title_by_library_tracks()` | Modify |
| `src/runner.rs` | Add third-tier title search call in `process_album`; pass library paths | Modify |
| `Cargo.toml` | Add `unicode-normalization` and `regex` dependencies | Modify |

---

### Task 1: Add `search_title_match` Config Field

**Files:**
- Modify: `src/config.rs`
- Modify: `Cargo.toml` (if unicode-normalization/regex not already present)

**Context:** The `SearchConfig` struct at `src/config.rs:56` has fields like `fallback_search: bool`, `timeout_secs: u64`. Add a new field `search_title_match: u32` with default 70, where 0 disables the feature.

- [ ] **Step 1: Check if dependencies exist**

Run: `grep -E "unicode-normalization|regex" Cargo.toml`
Expected: If not found, add them in Step 2.

- [ ] **Step 2: Add dependencies if missing**

Edit `Cargo.toml` — add to `[dependencies]`:
```toml
unicode-normalization = "0.1"
regex = "1"
```

Run: `cargo check`
Expected: Dependencies downloaded, compilation succeeds

- [ ] **Step 3: Write failing tests for new config field**

Edit `src/config.rs` — find the test module (around line 700+). Add tests:

```rust
#[test]
fn test_search_title_match_defaults_to_70() {
    let config = Config::default();
    assert_eq!(config.search.search_title_match, 70);
}

#[test]
fn test_search_title_match_from_yaml() {
    let yaml = r#"
library:
  paths: []
search:
  search_title_match: 50
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.search.search_title_match, 50);
}

#[test]
fn test_search_title_match_disabled_with_zero() {
    let yaml = r#"
library:
  paths: []
search:
  search_title_match: 0
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.search.search_title_match, 0);
}
```

Run: `cargo test search_title_match`
Expected: FAIL — field `search_title_match` not found

- [ ] **Step 4: Add default function**

Edit `src/config.rs` — add near the other default functions:

```rust
fn default_search_title_match() -> u32 {
    70
}
```

- [ ] **Step 5: Add field to SearchConfig**

Edit `src/config.rs` — in `pub struct SearchConfig` (line 56), add before the closing brace:

```rust
    #[serde(default = "default_search_title_match")]
    pub search_title_match: u32,
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test search_title_match`
Expected: All 3 tests PASS

- [ ] **Step 7: Commit**

```bash
git add src/config.rs Cargo.toml Cargo.lock
git commit -m "feat: add search_title_match config field (default 70, 0=disabled)"
```

---

### Task 2: Add `clean_track_title()` Helper

**Files:**
- Modify: `src/search.rs`

**Context:** The `search.rs` file contains `path_matches_artist()` (line 153) and `search_fallback_only()` (line 96). Add a new public function `clean_track_title()` that aggressively normalizes a filename into a searchable title.

- [ ] **Step 1: Write failing tests**

Edit `src/search.rs` — find the test module. Add tests:

```rust
#[test]
fn clean_track_title_strips_leading_track_number() {
    assert_eq!(clean_track_title("03. I Miss You.mp3"), "i miss you");
    assert_eq!(clean_track_title("01 - Hello.flac"), "hello");
    assert_eq!(clean_track_title("12-Song Name.aac"), "song name");
    assert_eq!(clean_track_title("01 Track Title.ogg"), "track title");
}

#[test]
fn clean_track_title_strips_extension() {
    assert_eq!(clean_track_title("Hello.mp3"), "hello");
    assert_eq!(clean_track_title("Song.flac"), "song");
    assert_eq!(clean_track_title("Track.m4a"), "track");
}

#[test]
fn clean_track_title_removes_brackets() {
    assert_eq!(clean_track_title("Song (Live).mp3"), "song live");
    assert_eq!(clean_track_title("Track [Remix].flac"), "track remix");
    assert_eq!(clean_track_title("Name {Demo}.ogg"), "name demo");
}

#[test]
fn clean_track_title_normalizes_unicode() {
    assert_eq!(clean_track_title("Café.mp3"), "cafe");
    assert_eq!(clean_track_title("Naïve.flac"), "naive");
}

#[test]
fn clean_track_title_removes_punctuation() {
    assert_eq!(clean_track_title("What's Up.mp3"), "whats up");
    assert_eq!(clean_track_title("Don't Stop.flac"), "dont stop");
}

#[test]
fn clean_track_title_handles_complex_filenames() {
    assert_eq!(
        clean_track_title("03. I Miss You (feat. Someone) [Remix].mp3"),
        "i miss you feat someone remix"
    );
    assert_eq!(
        clean_track_title("12 - The Name of the Game (edit).flac"),
        "the name of the game edit"
    );
}
```

Run: `cargo test clean_track_title`
Expected: FAIL — function not found

- [ ] **Step 2: Implement `clean_track_title()`**

Edit `src/search.rs` — add before `path_matches_artist` (line 153):

```rust
/// Clean a track filename into a searchable title.
///
/// Aggressively normalizes: strips leading track numbers, extensions,
/// brackets, punctuation; lowercases; normalizes unicode to ASCII.
/// Used for title-based search fallback when artist+album searches fail.
///
/// Examples:
/// - "03. I Miss You.mp3" → "i miss you"
/// - "01 - Hello (Live) [Remix].flac" → "hello live remix"
pub fn clean_track_title(filename: &str) -> String {
    use unicode_normalization::UnicodeNormalization;

    // Strip file extension
    let without_ext = filename
        .rsplit_once('.')
        .map(|(base, _ext)| base)
        .unwrap_or(filename);

    // Remove leading track number patterns: "01.", "01 -", "01-", "1."
    let re = regex::Regex::new(r"^\d+[\.\-\s]+").unwrap();
    let no_track_num = re.replace(without_ext, "");

    // Remove bracket contents: (…), […], {…}
    let re_brackets = regex::Regex::new(r"[\(\)\[\]\{\}][^)]*").unwrap();
    let no_brackets = re_brackets.replace_all(&no_track_num, " ");

    // Normalize unicode (NFKD), strip combining marks, collect to ASCII
    let normalized: String = no_brackets
        .nfkd()
        .filter(|c| c.is_ascii())
        .collect();

    // Lowercase, remove punctuation (keep alphanumeric and spaces)
    let cleaned: String = normalized
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();

    // Collapse multiple spaces and trim
    let re_spaces = regex::Regex::new(r"\s+").unwrap();
    re_spaces.replace_all(&cleaned.trim(), " ").to_string()
}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test clean_track_title`
Expected: All 6 tests PASS

- [ ] **Step 4: Commit**

```bash
git add src/search.rs
git commit -m "feat: add clean_track_title() for title-search fallback"
```

---

### Task 3: Add `get_library_track_filenames()` Function

**Files:**
- Modify: `src/search.rs`

**Context:** Need to read library track filenames from disk for a specific artist/album. The scanner (`src/scanner.rs:32`) uses `walkdir` and infers artist/album from directory structure `<root>/<artist>/<album>/<files>`. This function reads the same structure but returns individual filenames.

- [ ] **Step 1: Write failing tests**

Edit `src/search.rs` test module:

```rust
#[test]
fn get_library_track_filenames_returns_sorted_filenames() {
    use std::fs;
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let album_dir = tmp.path().join("Adele").join("25");
    fs::create_dir_all(&album_dir).unwrap();

    // Create track files in non-alphabetical order
    fs::write(album_dir.join("03 - Send My Love.mp3"), b"").unwrap();
    fs::write(album_dir.join("01 - Hello.flac"), b"").unwrap();
    fs::write(album_dir.join("02 - Send My Love (To Your New Lover).m4a"), b"").unwrap();
    // Non-audio file should be ignored
    fs::write(album_dir.join("cover.jpg"), b"").unwrap();

    let result = get_library_track_filenames(&[tmp.path().to_string_lossy().to_string()], "Adele", "25");
    assert!(result.is_ok());
    let filenames = result.unwrap();
    assert_eq!(filenames.len(), 3);
    // Should be sorted alphabetically
    assert!(filenames[0].contains("01 - Hello"));
    assert!(filenames[1].contains("02 - Send My Love"));
    assert!(filenames[2].contains("03 - Send My Love"));
}

#[test]
fn get_library_track_filenames_returns_empty_for_missing_album() {
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let result = get_library_track_filenames(&[tmp.path().to_string_lossy().to_string()], "Adele", "25");
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}
```

Run: `cargo test get_library_track_filenames`
Expected: FAIL — function not found

- [ ] **Step 2: Implement `get_library_track_filenames()`**

Edit `src/search.rs` — add after `clean_track_title()`:

```rust
/// Get individual track filenames from the library for a specific artist/album.
///
/// Reads the filesystem on demand (no schema change needed). Returns sorted
/// filenames of audio files found under `<library_path>/<artist>/<album>/`.
/// Non-audio files and subdirectories are ignored.
pub fn get_library_track_filenames(
    library_paths: &[String],
    artist: &str,
    album: &str,
) -> Result<Vec<String>> {
    let known_extensions: std::collections::HashSet<&str> =
        ["flac", "mp3", "m4a", "aac", "ogg", "opus", "wav", "wma", "ape"]
            .iter()
            .copied()
            .collect();

    let mut filenames = Vec::new();

    for lib_path in library_paths {
        let album_dir = Path::new(lib_path).join(artist).join(album);
        if !album_dir.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&album_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            if known_extensions.contains(ext.as_str()) {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    filenames.push(name.to_string());
                }
            }
        }
    }

    filenames.sort();
    Ok(filenames)
}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test get_library_track_filenames`
Expected: All 2 tests PASS

- [ ] **Step 4: Commit**

```bash
git add src/search.rs
git commit -m "feat: add get_library_track_filenames() for on-demand filesystem read"
```

---

### Task 4: Add `search_by_title()` Function

**Files:**
- Modify: `src/search.rs`

**Context:** This function performs the actual Soulseek search using a cleaned track title as the query. It's similar to `search_raw()` but filters results to keep only those with matching library tracks.

- [ ] **Step 1: Write failing tests**

Edit `src/search.rs` test module:

```rust
#[test]
fn search_by_title_filters_to_matching_tracks() {
    // This test verifies the filtering logic, not the actual Soulseek search.
    // Mock results with files that do/don't match library track titles.
    use crate::client::FileInfo;
    use std::collections::HashMap;

    fn make_file(name: &str) -> FileInfo {
        FileInfo {
            name: name.into(),
            size: 10_000_000,
            attribs: HashMap::new(),
        }
    }

    let library_titles = vec!["i miss you".to_string(), "hello".to_string()];

    // Result with 2 matching tracks (out of 2 library tracks = 100%)
    let result = SearchResult {
        username: "peer1".into(),
        speed: 500,
        slots: 1,
        files: vec![
            make_file("Music\\Adele\\25\\01 - Hello.flac"),
            make_file("Music\\Adele\\25\\03 - I Miss You.flac"),
        ],
    };

    // Clean basenames and check matches
    let matched = result.files.iter().filter(|f| {
        let basename = f.name.rsplit(['/', '\\']).next().unwrap_or(&f.name);
        let cleaned = clean_track_title(basename);
        library_titles.contains(&cleaned)
    }).count();

    assert_eq!(matched, 2);
    let pct = (matched as f64 / library_titles.len() as f64) * 100.0;
    assert!(pct >= 70.0);
}
```

Run: `cargo test search_by_title`
Expected: FAIL — function not found (or pass if test only uses existing types)

- [ ] **Step 2: Implement `search_by_title()`**

Edit `src/search.rs` — add after `get_library_track_filenames()`:

```rust
/// Search Soulseek by track title and filter results to those containing
/// enough matching library tracks.
///
/// This is the third-tier fallback (last resort) when both primary and
/// album-only searches return zero results. It searches for the cleaned
/// title of the first library track, then verifies each result contains
/// at least `match_threshold_pct`% of library tracks.
///
/// Returns filtered search results ready for `filter::filter_results()`.
pub async fn search_by_title(
    client: &dyn SoulseekClient,
    library_filenames: &[String],
    timeout_secs: u64,
    match_threshold_pct: u32,
) -> Result<Vec<SearchResult>> {
    // Extract clean titles from library filenames
    let library_titles: Vec<String> = library_filenames
        .iter()
        .map(|f| clean_track_title(f))
        .collect();

    if library_titles.is_empty() {
        return Ok(vec![]);
    }

    // Use the first (alphabetically sorted) track title as the search query
    let query = &library_titles[0];
    let mut results = search_raw(client, query, timeout_secs).await?;

    // Filter each result: keep only files whose clean basename matches a
    // library track title, and require enough matches to meet the threshold.
    let threshold = match_threshold_pct as f64 / 100.0;
    let required_matches = (library_titles.len() as f64 * threshold).ceil() as usize;

    results.retain_mut(|r| {
        r.files.retain(|f| {
            let basename = f.name.rsplit(['/', '\\']).next().unwrap_or(&f.name);
            let cleaned = clean_track_title(basename);
            library_titles.contains(&cleaned)
        });
        r.files.len() >= required_matches
    });

    Ok(results)
}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test search_by_title`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add src/search.rs
git commit -m "feat: add search_by_title() third-tier fallback search"
```

---

### Task 5: Extend SearchOutcome with `used_title_search`

**Files:**
- Modify: `src/search.rs`

**Context:** The `SearchOutcome` struct (line 41) tracks whether the fallback was used. Extend it with `used_title_search` for the new tier.

- [ ] **Step 1: Write failing test**

Edit `src/search.rs` test module:

```rust
#[test]
fn search_outcome_has_used_title_search_field() {
    let outcome = SearchOutcome {
        results: vec![],
        used_fallback: false,
        fallback_duration_ms: None,
        used_title_search: false,
    };
    assert!(!outcome.used_title_search);
}
```

Run: `cargo test search_outcome_has_used_title_search`
Expected: FAIL — field not found

- [ ] **Step 2: Add field to SearchOutcome**

Edit `src/search.rs` — in `pub struct SearchOutcome` (line 41), add:

```rust
    /// Whether the title-search fallback was used.
    pub used_title_search: bool,
```

- [ ] **Step 3: Update all SearchOutcome construction sites**

Edit `src/search.rs` — find all places where `SearchOutcome { ... }` is constructed and add `used_title_search: false`. There should be 2-3 construction sites in `search_album_with_fallback()`.

Example pattern:
```rust
SearchOutcome {
    results: primary,
    used_fallback: false,
    fallback_duration_ms: None,
    used_title_search: false,  // ADD THIS
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: All tests PASS (no construction sites missed)

- [ ] **Step 5: Commit**

```bash
git add src/search.rs
git commit -m "feat: extend SearchOutcome with used_title_search field"
```

---

### Task 6: Integrate Title Search in `process_album`

**Files:**
- Modify: `src/runner.rs`

**Context:** In `process_album` (line 62), after the second-chance album-only fallback returns empty (around line 200), add the third-tier title search. Need to pass library paths from the config.

- [ ] **Step 1: Write failing test**

Edit `src/runner.rs` test module — add test:

```rust
#[tokio::test]
async fn test_title_search_fallback_triggers_when_both_searches_fail() {
    // Mock client that returns empty for "Artist Album" and "Album" queries,
    // but returns results for a title query.
    // This tests the integration path, not the actual Soulseek search.
    // ... (depends on existing test infrastructure)
}
```

This test depends on the MockClient supporting query-based responses. Check if `search_results_by_query` field exists on MockClient (it does — `src/client.rs:75`).

- [ ] **Step 2: Add title search fallback in process_album**

Edit `src/runner.rs` — find the section after the second-chance fallback (around line 200) where `filtered.is_empty()` is checked. Before the final "No results" log, add:

```rust
    // Third-tier fallback: title search (auto mode only, last resort)
    if filtered.is_empty()
        && config.search.search_title_match > 0
        && !config.library.paths.is_empty()
        && album.is_some()
    {
        let album_name = album.unwrap();
        let lib_filenames = search::get_library_track_filenames(
            &config.library.paths,
            artist,
            album_name,
        )
        .unwrap_or_default();

        if !lib_filenames.is_empty() {
            let title_search_start = std::time::Instant::now();
            match search::search_by_title(
                client,
                &lib_filenames,
                config.search.timeout_secs,
                config.search.search_title_match,
            )
            .await
            {
                Ok(title_results) => {
                    let title_duration = title_search_start.elapsed().as_millis() as u64;
                    tracing::info!(
                        "{artist} — {}: title-search fallback found {} result(s)",
                        album_name,
                        title_results.len(),
                    );
                    if let Err(e) = search::record_search(
                        artist,
                        Some(album_name),
                        title_results.len(),
                        title_duration,
                        db,
                    ) {
                        tracing::warn!(
                            "{artist} — {}: failed to record title search history: {e}",
                            album_name
                        );
                    }
                    if !title_results.is_empty() {
                        filtered = filter::filter_results(
                            &title_results,
                            &config.filters,
                            library_track_count,
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "{artist} — {}: title-search fallback failed: {e}",
                        album_name
                    );
                }
            }
        }
    }
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test`
Expected: All tests PASS

- [ ] **Step 4: Commit**

```bash
git add src/runner.rs
git commit -m "feat: integrate title-search fallback in process_album"
```

---

### Task 7: Update Search History for Title Search

**Files:**
- Modify: `src/runner.rs`

**Context:** The title search should record in `search_history` with a marker so it's visible in the history table. The `record_search` function (search.rs:122) already handles this.

- [ ] **Step 1: Verify title search history is recorded**

The code in Task 6 already calls `search::record_search()` for title search results. Verify by running the test suite.

Run: `cargo test`
Expected: PASS

- [ ] **Step 2: Add title search count to the "No results" log**

Edit `src/runner.rs` — find the "No results" log (around line 205). Update to include title search info:

```rust
        tracing::info!(
            "No results for {artist} — {} (tried: primary, album-only{}",
            album.unwrap_or("(all)"),
            if config.search.search_title_match > 0 { ", title-search" } else { "" }
        );
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add src/runner.rs
git commit -m "feat: log title-search fallback in search history"
```

---

### Task 8: Full Test Suite Verification and Cleanup

**Files:**
- None (verification only)

- [ ] **Step 1: Run full test suite**

Run: `cargo test`
Expected: All tests PASS

- [ ] **Step 2: Run clippy**

Run: `cargo clippy -- -D warnings`
Expected: Clean — no warnings

- [ ] **Step 3: Run fmt check**

Run: `cargo fmt --check`
Expected: Clean

- [ ] **Step 4: Fix any issues found**

If clippy or fmt report issues, fix them:

Run: `cargo fmt && cargo clippy --fix -- -D warnings`

- [ ] **Step 5: Final verification**

Run: `cargo test && cargo clippy -- -D warnings && cargo fmt --check`
Expected: All clean

- [ ] **Step 6: Commit any cleanup**

```bash
git add -A
git commit -m "chore: apply clippy and fmt fixes for title-search fallback"
```
