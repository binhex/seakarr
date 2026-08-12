# Fallback Album Search Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When the combined "Artist Album" search returns zero results, fall back to an album-only search and accept results whose share-relative file paths match the artist (word-level), configurable via `search.fallback_search` (default on).

**Architecture:** A new `search_album_with_fallback()` in `search.rs` wraps the existing `search_album()` primitive. The runner swaps one call and records both searches in `search_history`. The rest of the pipeline (filter → rank → download → organize → notify) is untouched.

**Tech Stack:** Rust, tokio, soulseek-rs-lib (vendored), serde/serde_yaml, rusqlite, tracing.

---

## Spec

Design spec (source of truth): `docs/specs/2026-08-12-fallback-album-search-design.md`

Decisions locked in the spec:

- Fallback fires **only** when the primary combined search returns zero **raw** results, `album` is `Some`, and `search.fallback_search` is `true`.
- Artist matching is **word-level**: artist is tokenised into alphanumeric words; common articles (`the`, `a`, `an`) are dropped; **all** remaining words must appear as case-insensitive substrings in the path (path lowercased, `\` normalised to `/`). If no words remain after stop-word removal, fall back to full lowercased artist name as a substring.
- A result passes if **any** of its files' paths match the artist.
- The fallback search and the primary search are both recorded in `search_history` (the primary row gets `result_count` 0 when the fallback fires; the fallback row gets its matched count). No schema change.
- Fallback search errors propagate as `Err`, like primary search errors.

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `src/config.rs` | Modify | `SearchConfig.fallback_search: bool` (serde default `true`), `Default` impl, test YAML |
| `src/client.rs` | Modify | `MockClient` test infra: per-query results + query recording |
| `src/search.rs` | Modify | `path_matches_artist()`, `search_album_with_fallback()`, `SearchOutcome`, dedup helper extraction |
| `src/runner.rs` | Modify | Call the new function, log fallback usage, record searches in `search_history` |

No new files. No CLI changes. No DB migration.

---

### Task 1: Config — `search.fallback_search` toggle

**Files:**
- Modify: `src/config.rs` (struct at ~line 56, `Default` impl at ~line 480, `sample_yaml()` at ~line 543, tests at ~line 660)
- Test: `src/config.rs` (tests module)

- [ ] **Step 1: Write the failing tests**

Add these two tests inside the `#[cfg(test)] mod tests` block in `src/config.rs`, next to `test_load_config_from_yaml`:

```rust
    #[test]
    fn test_fallback_search_defaults_true() {
        let config = Config::default();
        assert!(config.search.fallback_search);
    }

    #[test]
    fn test_fallback_search_from_yaml() {
        let dir = TempDir::new().unwrap();
        let yaml_path = dir.path().join("seakarr.yml");
        fs::write(&yaml_path, sample_yaml()).unwrap();

        let config = Config::load(dir.path()).unwrap();

        assert!(config.search.fallback_search);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p seakarr test_fallback_search`
Expected: FAIL — compile error `no field 'fallback_search' on type 'SearchConfig'`.

- [ ] **Step 3: Add the field to `SearchConfig`**

In `src/config.rs`, inside `pub struct SearchConfig`, after the `block_pause_secs` field add:

```rust
    #[serde(default = "default_true")]
    pub fallback_search: bool,
```

(`default_true()` already exists in this file — used by `scan_on_startup`.)

- [ ] **Step 4: Add the field to the manual `Default` impl**

In `impl Default for Config`, inside the `search: SearchConfig { ... }` block, after `block_pause_secs: default_block_pause(),` add:

```rust
                fallback_search: default_true(),
```

- [ ] **Step 5: Add the key to `sample_yaml()`**

In the test helper `sample_yaml()`, in the `search:` section, after `block_pause_secs: 300` add:

```yaml
  fallback_search: true
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p seakarr test_fallback_search`
Expected: PASS (2 tests).

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add src/config.rs
git commit -m "feat: add search.fallback_search config toggle (default on)"
```

---

### Task 2: MockClient test infrastructure — per-query results and query recording

The fallback logic must be testable without a live network. `MockClient::search` currently returns the same static `search_results` for every query, which makes "did a second search happen?" untestable.

**Files:**
- Modify: `src/client.rs` (struct at ~line 72, `new()` at ~line 84, `search()` at ~line 138)
- Test: `src/client.rs` (tests module)

- [ ] **Step 1: Write the failing test**

Add this test to the tests module in `src/client.rs`:

```rust
    #[tokio::test]
    async fn test_mock_search_records_queries_and_per_query_results() {
        let client = MockClient::new();
        client
            .search_results_by_query
            .lock()
            .unwrap()
            .insert(
                "history".into(),
                vec![mock_search_result(
                    "peer1",
                    500,
                    1,
                    vec![("01 - track.flac", 10_000_000, 900)],
                )],
            );

        // Per-query override applies.
        let by_query = client.search("history", 15).await.unwrap();
        assert_eq!(by_query.len(), 1);
        assert_eq!(by_query[0].username, "peer1");

        // No override -> falls back to the static search_results set.
        *client.search_results.lock().unwrap() = vec![mock_search_result(
            "static_peer",
            500,
            1,
            vec![("02 - track.flac", 10_000_000, 900)],
        )];
        let by_fallback = client.search("no override", 15).await.unwrap();
        assert_eq!(by_fallback[0].username, "static_peer");

        // Every query is recorded, in order.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(queries, vec!["history".to_string(), "no override".to_string()]);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p seakarr test_mock_search_records`
Expected: FAIL — compile error: `no field 'search_results_by_query' on type 'MockClient'`.

- [ ] **Step 3: Add the fields to the `MockClient` struct**

In `src/client.rs`, in `pub struct MockClient`, after the `search_results` field add:

```rust
    /// Per-query override map. When a query has an entry here, `search()`
    /// returns it instead of the static `search_results`.
    pub search_results_by_query: Mutex<HashMap<String, Vec<SearchResult>>>,
    /// Every query string passed to `search()`, in call order.
    pub search_queries: Mutex<Vec<String>>,
```

- [ ] **Step 4: Initialise them in `new()`**

In `MockClient::new()`, after `search_results: Mutex::new(vec![]),` add:

```rust
            search_results_by_query: Mutex::new(HashMap::new()),
            search_queries: Mutex::new(vec![]),
```

- [ ] **Step 5: Update `search()`**

Replace the body of `MockClient::search` (currently returns the static results) with:

```rust
    async fn search(&self, query: &str, _timeout_secs: u64) -> Result<Vec<SearchResult>> {
        self.search_queries.lock().unwrap().push(query.to_string());
        if let Some(results) = self.search_results_by_query.lock().unwrap().get(query) {
            return Ok(results.clone());
        }
        Ok(self.search_results.lock().unwrap().clone())
    }
```

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test -p seakarr test_mock_search_records`
Expected: PASS. Also run the existing suite to confirm no regression: `cargo test -p seakarr` — all PASS.

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add src/client.rs
git commit -m "test: add per-query results and query recording to MockClient"
```

---

### Task 3: `path_matches_artist` — word-level artist matching

**Files:**
- Modify: `src/search.rs` (add function + tests)

- [ ] **Step 1: Write the failing tests**

Add these tests to the tests module in `src/search.rs`:

```rust
    #[test]
    fn test_path_matches_artist_basic_backslash_path() {
        assert!(path_matches_artist(
            r"@@rldqn\complete\Michael Jackson\History\01 - Billie Jean.flac",
            "Michael Jackson"
        ));
    }

    #[test]
    fn test_path_matches_artist_case_insensitive_and_forward_slashes() {
        assert!(path_matches_artist(
            "music/michael jackson/history/01 - billie jean.flac",
            "MICHAEL JACKSON"
        ));
    }

    #[test]
    fn test_path_matches_artist_reordered_words() {
        assert!(path_matches_artist(
            "Jackson, Michael - History - 01 - Billie Jean.flac",
            "Michael Jackson"
        ));
    }

    #[test]
    fn test_path_matches_artist_dropped_article() {
        // Artist "The Beatles" must match a path shared as just "Beatles".
        assert!(path_matches_artist(
            "Beatles - Abbey Road - 01 - Come Together.flac",
            "The Beatles"
        ));
    }

    #[test]
    fn test_path_matches_artist_punctuation() {
        assert!(path_matches_artist(
            r"AC-DC\Back in Black\01 - Hells Bells.flac",
            "AC/DC"
        ));
    }

    #[test]
    fn test_path_matches_artist_all_stop_words_falls_back_to_full_name() {
        assert!(path_matches_artist(
            "The The - Infected - 01.flac",
            "The The"
        ));
        assert!(!path_matches_artist(
            "Some Other Artist - 01.flac",
            "The The"
        ));
    }

    #[test]
    fn test_path_matches_artist_no_match() {
        assert!(!path_matches_artist(
            r"Music\Other Artist\History\01 - track.flac",
            "Michael Jackson"
        ));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p seakarr path_matches_artist`
Expected: FAIL — compile error: `cannot find function 'path_matches_artist'`.

- [ ] **Step 3: Implement the function**

Add to `src/search.rs`, below the `record_search` function (before the tests module):

```rust
/// Common articles that carry no discriminating power when matching an
/// artist against a file path ("The Beatles" must match "Beatles").
const ARTIST_STOP_WORDS: &[&str] = &["the", "a", "an"];

/// Check whether a share-relative file path matches the artist, word-level.
///
/// The path is lowercased and `\` separators normalised to `/`. The artist
/// is split into alphanumeric words; common articles are dropped and every
/// remaining word must appear as a case-insensitive substring of the path.
/// If no words remain (artist is all stop-words), the full lowercased
/// artist name is matched as a substring instead.
///
/// Known accepted risk: substring-per-word means "Prince" also matches
/// "Princess". Downstream quality filters still apply.
pub fn path_matches_artist(path: &str, artist: &str) -> bool {
    let normalised = path.to_lowercase().replace('\\', "/");
    let words: Vec<String> = artist
        .split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    let distinctive: Vec<&String> = words
        .iter()
        .filter(|w| !ARTIST_STOP_WORDS.contains(&w.as_str()))
        .collect();
    if distinctive.is_empty() {
        return normalised.contains(&artist.to_lowercase());
    }
    distinctive.iter().all(|w| normalised.contains(w.as_str()))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p seakarr path_matches_artist`
Expected: PASS (7 tests).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/search.rs
git commit -m "feat: add word-level artist-to-path matching helper"
```

---

### Task 4: `search_album_with_fallback` and `SearchOutcome`

This refactors `search.rs` slightly: the dedup loop becomes a shared private helper, a `search_raw` primitive takes a raw query, `search_album` builds the combined query and delegates, and the new fallback function orchestrates the two searches.

**Files:**
- Modify: `src/search.rs`
- Test: `src/search.rs`

- [ ] **Step 1: Write the failing tests**

Add these tests to the tests module in `src/search.rs` (note: the primary query string is `format!("{artist} {a}")`, so for artist `Michael Jackson` and album `History` it is `"Michael Jackson History"`; the fallback query is the album name verbatim, `"History"`):

```rust
    #[tokio::test]
    async fn test_fallback_used_when_primary_empty() {
        let client = MockClient::new();
        client.search_results_by_query.lock().unwrap().insert(
            "History".into(),
            vec![SearchResult {
                username: "peer1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    r"Music\Michael Jackson\History\01 - Billie Jean.flac",
                    900,
                    30_000_000,
                )],
            }],
        );
        // The primary query has no map entry, so it returns the empty
        // static search_results — the fallback trigger.

        let outcome =
            search_album_with_fallback(&client, "Michael Jackson", Some("History"), 15, true)
                .await
                .unwrap();
        assert!(outcome.used_fallback);
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].username, "peer1");
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec!["Michael Jackson History".to_string(), "History".to_string()]
        );
    }

    #[tokio::test]
    async fn test_no_fallback_when_primary_non_empty() {
        let client = MockClient::new();
        *client.search_results.lock().unwrap() = vec![SearchResult {
            username: "user1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file("track.flac", 900, 30_000_000)],
        }];

        let outcome =
            search_album_with_fallback(&client, "Artist", Some("Album"), 15, true)
                .await
                .unwrap();
        assert!(!outcome.used_fallback);
        assert_eq!(outcome.results.len(), 1);
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(queries, vec!["Artist Album".to_string()]);
    }

    #[tokio::test]
    async fn test_fallback_skipped_when_disabled() {
        let client = MockClient::new();
        let outcome =
            search_album_with_fallback(&client, "Artist", Some("Album"), 15, false)
                .await
                .unwrap();
        assert!(!outcome.used_fallback);
        assert_eq!(client.search_queries.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_fallback_skipped_when_no_album() {
        let client = MockClient::new();
        let outcome = search_album_with_fallback(&client, "Artist", None, 15, true)
            .await
            .unwrap();
        assert!(!outcome.used_fallback);
        assert_eq!(client.search_queries.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_fallback_filters_results_by_artist_in_path() {
        let client = MockClient::new();
        client.search_results_by_query.lock().unwrap().insert(
            "Album".into(),
            vec![
                SearchResult {
                    username: "right".into(),
                    speed: 500,
                    slots: 1,
                    files: vec![make_file(
                        r"Music\Artist\Album\01 - track.flac",
                        900,
                        30_000_000,
                    )],
                },
                SearchResult {
                    username: "wrong".into(),
                    speed: 999,
                    slots: 1,
                    files: vec![make_file(
                        r"Music\Other Artist\Album\01 - track.flac",
                        900,
                        30_000_000,
                    )],
                },
            ],
        );

        let outcome =
            search_album_with_fallback(&client, "Artist", Some("Album"), 15, true)
                .await
                .unwrap();
        assert!(outcome.used_fallback);
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].username, "right");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p seakarr search_album_with_fallback`
Expected: FAIL — compile error: `cannot find struct 'SearchOutcome'` / `cannot find function 'search_album_with_fallback'`.

- [ ] **Step 3: Extract the dedup helper and add `search_raw`**

In `src/search.rs`, replace the current `search_album` implementation with:

```rust
/// Search Soulseek with a raw query, returning deduplicated results.
async fn search_raw(
    client: &dyn SoulseekClient,
    query: &str,
    timeout_secs: u64,
) -> Result<Vec<SearchResult>> {
    let mut results = client.search(query, timeout_secs).await?;
    dedup_results(&mut results);
    Ok(results)
}

/// Deduplicate by filename+size within each result's files.
fn dedup_results(results: &mut [SearchResult]) {
    for result in results {
        result.files.sort_by(|a, b| a.name.cmp(&b.name));
        result
            .files
            .dedup_by(|a, b| a.name == b.name && a.size == b.size);
    }
}

/// Search Soulseek for an album, returning deduplicated results.
pub async fn search_album(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    timeout_secs: u64,
) -> Result<Vec<SearchResult>> {
    let query = match album {
        Some(a) if !a.is_empty() => format!("{artist} {a}"),
        _ => artist.to_string(),
    };
    search_raw(client, &query, timeout_secs).await
}
```

- [ ] **Step 4: Add `SearchOutcome` and `search_album_with_fallback`**

Add below `search_album` (above `record_search`):

```rust
/// Outcome of an album search, including whether the fallback ran.
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub results: Vec<SearchResult>,
    pub used_fallback: bool,
}

/// Search for an album, falling back to an album-only query when the
/// combined "Artist Album" search returns zero results.
///
/// Soulseek sometimes bans specific artist+album criteria. The fallback
/// searches by album name alone and keeps only results where at least one
/// file's share-relative path matches the artist (see
/// [`path_matches_artist`]), so the download pipeline receives the same
/// quality-filtered candidates as a normal search.
pub async fn search_album_with_fallback(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    timeout_secs: u64,
    fallback_enabled: bool,
) -> Result<SearchOutcome> {
    let primary = search_album(client, artist, album, timeout_secs).await?;
    let Some(album_name) = album.filter(|a| !a.is_empty()) else {
        return Ok(SearchOutcome {
            results: primary,
            used_fallback: false,
        });
    };
    if !primary.is_empty() || !fallback_enabled {
        return Ok(SearchOutcome {
            results: primary,
            used_fallback: false,
        });
    }

    let mut fallback = search_raw(client, album_name, timeout_secs).await?;
    fallback.retain(|r| {
        r.files
            .iter()
            .any(|f| path_matches_artist(&f.name, artist))
    });
    Ok(SearchOutcome {
        results: fallback,
        used_fallback: true,
    })
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p seakarr search_album_with_fallback`
Expected: PASS (5 new tests). Then run the whole search module: `cargo test -p seakarr search::` — the two pre-existing `test_search_*` tests must still PASS.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add src/search.rs
git commit -m "feat: add album-only fallback search with artist-in-path matching"
```

---

### Task 5: Wire fallback + search recording into `runner.rs`

**Files:**
- Modify: `src/runner.rs` (`process_album` search block at ~line 43)
- Test: `src/runner.rs` (tests module)

- [ ] **Step 1: Write the failing tests**

Add these tests to the tests module in `src/runner.rs` (after `test_run_manual_mode`), reusing the existing `make_file` and `make_test_config` helpers:

```rust
    #[tokio::test]
    async fn test_fallback_download_completes_album_and_records_history() {
        let client = Arc::new(MockClient::new());
        client.search_results_by_query.lock().unwrap().insert(
            "Test Album".into(),
            vec![SearchResult {
                username: "user1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    r"Music\Test Artist\Test Album\01 - track.flac",
                    900,
                    10_000_000,
                )],
            }],
        );
        // Primary query "Test Artist Test Album" has no map entry -> empty.

        let config = make_test_config();
        let db = Database::open_in_memory().unwrap();
        let staging = TempDir::new().unwrap();

        process_album(
            client.as_ref() as &dyn crate::client::SoulseekClient,
            "Test Artist",
            Some("Test Album"),
            &config,
            &db,
            staging.path(),
        )
        .await
        .unwrap();

        // Fallback fired: primary query then album-only query.
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Test Artist Test Album".to_string(),
                "Test Album".to_string()
            ]
        );

        // Album completed successfully.
        let rows = db.get_processed_albums().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "success");

        // Both searches recorded: primary with 0 results, fallback with 1.
        let history_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM search_history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(history_count, 2);
        let fallback_count: i64 = db
            .conn
            .query_row(
                "SELECT result_count FROM search_history WHERE album = 'Test Album' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fallback_count, 1);
    }

    #[tokio::test]
    async fn test_fallback_no_matches_marks_skipped() {
        let client = Arc::new(MockClient::new());
        client.search_results_by_query.lock().unwrap().insert(
            "Test Album".into(),
            vec![SearchResult {
                username: "user1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    r"Music\Some Other Artist\Test Album\01 - track.flac",
                    900,
                    10_000_000,
                )],
            }],
        );

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

        let rows = db.get_processed_albums().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "skipped");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p seakarr test_fallback_`
Expected: FAIL — the first test fails because only one query is issued (no fallback yet) and no history rows exist; the second fails because `search_results_by_query` is ignored (album downloads from static empty results → marked `skipped`... it may pass by accident for the wrong reason — after the implementation step both must pass for the right reason).

- [ ] **Step 3: Replace the search block in `process_album`**

In `src/runner.rs`, replace this block:

```rust
    // Search
    let results = search::search_album(client, artist, album, config.search.timeout_secs).await?;
    if results.is_empty() {
        tracing::info!("No results for {artist} — {}", album.unwrap_or("(all)"));
        if let Some(a) = album {
            db.mark_album_processed(artist, a, "skipped")?;
        }
        return Ok(());
    }
```

with:

```rust
    // Search, with an album-only fallback for banned artist+album criteria.
    // Both searches are recorded in search_history; when the fallback fires
    // the primary row gets result_count 0 and the fallback row its matched
    // count, making fallback usage visible in the history table.
    let search_start = std::time::Instant::now();
    let outcome = search::search_album_with_fallback(
        client,
        artist,
        album,
        config.search.timeout_secs,
        config.search.fallback_search,
    )
    .await?;
    let duration_ms = search_start.elapsed().as_millis() as u64;
    search::record_search(
        artist,
        album,
        if outcome.used_fallback {
            0
        } else {
            outcome.results.len()
        },
        duration_ms,
        db,
    )?;
    if outcome.used_fallback {
        tracing::info!(
            "{artist} — {}: fallback album-only search found {} result(s) matching artist in path",
            album.unwrap_or("(all)"),
            outcome.results.len(),
        );
        search::record_search(artist, album, outcome.results.len(), duration_ms, db)?;
    }
    let results = outcome.results;
    if results.is_empty() {
        tracing::info!("No results for {artist} — {}", album.unwrap_or("(all)"));
        if let Some(a) = album {
            db.mark_album_processed(artist, a, "skipped")?;
        }
        return Ok(());
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p seakarr test_fallback_`
Expected: PASS (2 tests). Then run the whole runner module: `cargo test -p seakarr runner::` — `test_run_manual_mode` and `test_runner_handles_empty_targets` must still PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/runner.rs
git commit -m "feat: wire fallback search into runner and record search history"
```

---

### Task 6: Full verification and final commit

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt --check`
Expected: no output, exit 0. If not, run `cargo fmt` and commit the formatting.

- [ ] **Step 2: Lint**

Run: `cargo clippy -p seakarr --all-targets -- -D warnings`
Expected: exit 0, no warnings.

- [ ] **Step 3: Full test suite (workspace includes the vendored soulseek-rs-lib)**

Run: `cargo test`
Expected: all tests PASS, including the vendored crate's regression tests.

- [ ] **Step 4: Final commit (if anything changed in Steps 1-3)**

```bash
git add -A
git commit -m "chore: final verification pass for fallback search"
```

(If nothing changed, skip the commit.)

---

## Self-Review Notes

- **Spec coverage:** fallback trigger (§1) → Task 4/5; word-level matching with stop-words (§2) → Task 3; config toggle (§3/§6) → Task 1; search history recording, primary + fallback (§3, amended) → Task 5; error propagation (§5) → `?` propagation in Tasks 4/5; testing matrix (§7) → Tasks 1-5.
- **Type consistency:** `SearchOutcome { results: Vec<SearchResult>, used_fallback: bool }` used identically in `search.rs` and `runner.rs`; `search_album_with_fallback(client, artist, album, timeout_secs, fallback_enabled)` signature consistent everywhere; `record_search(artist, album, result_count: usize, duration_ms: u64, db)` matches the existing signature; `MockClient` fields `search_results_by_query` and `search_queries` match between struct, `new()`, `search()`, and tests.
- **Existing tests preserved:** `search_album` behaviour is unchanged (Task 4 refactor delegates to `search_raw` with the same query construction and dedup), so `test_search_returns_results` and `test_search_deduplicates_by_filename` still pass.
