# Filter-Aware Search Cascade Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the search fallback cascade continue through the lowercase, punctuation-normalised, and album-only tiers until a tier yields results that survive the full filter pipeline, instead of stopping at the first tier with any raw results.

**Architecture:** Add `filters: &FilterConfig` and `library_track_count: Option<usize>` to `search_album_with_fallback`. Each tier probes its output with the real `filter::filter_results`; a tier is accepted only when the probe is non-empty. If a tier's results are non-empty but all rejected, remember the first such tier as a fallback and continue. Return the first usable tier, else the first non-empty tier, else empty.

**Tech Stack:** Rust (async/`tokio`), `crate::filter::filter_results`, `crate::config::FilterConfig`, `tracing` for logs.

---

## File Structure

| File | Responsibility | Change |
|------|----------------|--------|
| `src/search.rs` | Search cascade | Add `filters`/`library_track_count` params; add `tier_has_usable_results` helper; probe each tier + fallback tracking; update doc comment; update tests (signature + new cases) |
| `src/runner.rs` | Orchestration | Call site passes `&config.filters` and `library_track_count` (one line) |

Only two files change. `filter.rs` already exports `filter_results` and does not import `search.rs`, so there is no dependency cycle.

---

### Task 1: Plumb `filters` + `library_track_count` through the cascade (behaviour-preserving)

This task changes the signature and updates every call site but does NOT change behaviour yet — the new params are prefixed `_` (unused) so the cascade still stops at the first non-empty raw tier. Existing tests must still pass.

**Files:**
- Modify: `src/search.rs` (signature, imports, test helper, 20 test call sites)
- Modify: `src/runner.rs` (call site)

- [ ] **Step 1: Add the import and change the signature**

In `src/search.rs`, add the `FilterConfig` import after the existing `use crate::client::...` line (top of file):

```rust
use crate::client::{SearchResult, SoulseekClient};
use crate::config::FilterConfig;
use crate::error::Result;
```

Change the signature (keep the body unchanged for now — only the two new `_`-prefixed params are added):

```rust
pub async fn search_album_with_fallback(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    timeout_secs: u64,
    _filters: &FilterConfig,
    _library_track_count: Option<usize>,
) -> Result<SearchOutcome> {
```

- [ ] **Step 2: Add a `test_filters()` helper to the test module**

Inside `mod tests` (which already has `use super::*;`), add this helper near the existing `make_file` helper. `contiguous_tracks: false` is required because the existing mock files (`track.flac`) have no parseable track number, which the contiguity check would reject.

```rust
    fn test_filters() -> FilterConfig {
        FilterConfig {
            allowed_extensions: vec!["flac".into()],
            min_bit_rate: 0,
            min_bit_depth: 0,
            exclude_words: vec![],
            include_locked: false,
            contiguous_tracks: false,
            min_tracks: 0,
            peer_track_count: false,
        }
    }
```

- [ ] **Step 3: Update the runner call site**

In `src/runner.rs` (the `process_album` function, ~line 137), replace:

```rust
    let outcome =
        search::search_album_with_fallback(client, artist, album, config.search.timeout_secs)
            .await?;
```

with:

```rust
    let outcome = search::search_album_with_fallback(
        client,
        artist,
        album,
        config.search.timeout_secs,
        &config.filters,
        library_track_count,
    )
    .await?;
```

`library_track_count` is a parameter of `process_album` and is already in scope here.

- [ ] **Step 4: Update all 20 test call sites**

Every `search_album_with_fallback(...)` call inside `mod tests` (currently 20 sites; e.g. `let outcome = search_album_with_fallback(&client, "Prince", Some("Musicology"), 15)`) must gain two trailing arguments: `&test_filters(), None`.

Before:

```rust
let outcome = search_album_with_fallback(&client, "Prince", Some("Musicology"), 15)
    .await
    .unwrap();
```

After:

```rust
let outcome = search_album_with_fallback(&client, "Prince", Some("Musicology"), 15, &test_filters(), None)
    .await
    .unwrap();
```

Apply the same `, &test_filters(), None` suffix to the other 19 sites (including the multi-line call near the end of the punctuation tests, which already spans several lines — add the two args to its argument list).

- [ ] **Step 5: Run the full test suite to confirm no behaviour change**

Run: `cargo test --workspace`
Expected: PASS — all tests green (count unchanged; the `_`-prefixed params are unused so clippy stays clean). Run `cargo clippy --workspace --all-targets -- -D warnings` — expect exit 0.

- [ ] **Step 6: Commit**

```bash
git add src/search.rs src/runner.rs
git commit -m "refactor: plumb filter config into search cascade signature"
```

---

### Task 2: Continue the cascade until a tier survives filtering (TDD)

**Files:**
- Modify: `src/search.rs` (helper, fallback logic, doc comment)
- Test: `src/search.rs` (2 new tests)

- [ ] **Step 1: Write the failing tests**

Append these two tests inside `mod tests` (after the existing punctuation-fallback tests). They reference the already-plumbed signature from Task 1 and assert the NEW behaviour, so they will fail until Task 2's implementation lands.

```rust
    // ── filter-aware cascade continuation ──

    #[tokio::test]
    async fn test_cascade_continues_when_primary_only_mp3() {
        let client = MockClient::new();
        // Tier 1a returns only mp3 (rejected by the flac filter) -> cascade
        // continues. Tier 1c returns flac and wins.
        client.search_results_by_query.lock().unwrap().insert(
            "S.P.Y. In The Skys".into(),
            vec![SearchResult {
                username: "mp3peer".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file("S.P.Y./In The Skys/01.mp3", 320, 8_000_000)],
            }],
        );
        client.search_results_by_query.lock().unwrap().insert(
            "SPY In The Skys".into(),
            vec![SearchResult {
                username: "flacpeer".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file("SPY/In The Skys/01 - Track.flac", 900, 30_000_000)],
            }],
        );

        let outcome =
            search_album_with_fallback(&client, "S.P.Y.", Some("In The Skys"), 15, &test_filters(), None)
                .await
                .unwrap();
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].username, "flacpeer");
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "S.P.Y. In The Skys".to_string(),
                "s.p.y. in the skys".to_string(),
                "SPY In The Skys".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn test_cascade_returns_first_tier_when_all_tiers_junk() {
        let client = MockClient::new();
        // Every tier returns only mp3 -> no tier passes the flac filter. The
        // cascade must return the FIRST tier's results (today's behaviour)
        // and still run all tiers.
        let mp3 = |path: &str| {
            vec![SearchResult {
                username: "mp3peer".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(path, 320, 8_000_000)],
            }]
        };
        client
            .search_results_by_query
            .lock()
            .unwrap()
            .insert("Prince Musicology".into(), mp3("Prince/Musicology/01.mp3"));
        client
            .search_results_by_query
            .lock()
            .unwrap()
            .insert("prince musicology".into(), mp3("Prince/Musicology/01.mp3"));
        client
            .search_results_by_query
            .lock()
            .unwrap()
            .insert("Musicology".into(), mp3("Prince/Musicology/01.mp3"));

        let outcome =
            search_album_with_fallback(&client, "Prince", Some("Musicology"), 15, &test_filters(), None)
                .await
                .unwrap();
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].username, "mp3peer");
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Prince Musicology".to_string(),
                "prince musicology".to_string(),
                "Musicology".to_string()
            ]
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test cascade_continues_when_primary_only_mp3 cascade_returns_first_tier`
Expected: FAIL — `test_cascade_continues_when_primary_only_mp3` returns 0 results (the cascade currently returns Tier 1a's mp3, but the assertion expects `flacpeer`); `test_cascade_returns_first_tier_when_all_tiers_junk` may pass already (it matches today's behaviour) — the first test is the RED signal.

- [ ] **Step 3: Implement the filter-aware continuation**

Add this helper to `src/search.rs` (just above `search_album_with_fallback`):

```rust
/// Returns true when `results` contain at least one result that passes the
/// full filter pipeline — i.e. when the tier produced a downloadable result
/// set. The cascade continues to the next tier when a tier returns non-empty
/// but unusable results.
fn tier_has_usable_results(
    results: &[SearchResult],
    filters: &FilterConfig,
    library_track_count: Option<usize>,
    album: Option<&str>,
) -> bool {
    !crate::filter::filter_results(results, filters, library_track_count, album).is_empty()
}
```

Rename the two params (`_filters` → `filters`, `_library_track_count` → `library_track_count`) and replace the entire function body with the following (the four tiers are unchanged except each early-return is gated by the probe, and a `fallback` tracks the first non-empty tier):

```rust
pub async fn search_album_with_fallback(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    timeout_secs: u64,
    filters: &FilterConfig,
    library_track_count: Option<usize>,
) -> Result<SearchOutcome> {
    // The first tier whose raw results were non-empty but did not survive
    // filtering. Returned when no tier yields a usable (filter-passing)
    // result set, so the caller's rejection summary and title-search
    // fallback still have data to work with.
    let mut fallback: Option<Vec<SearchResult>> = None;

    // Tier 1a: primary "Artist Album" search (original casing)
    if let Some(a) = album.filter(|a| !a.trim().is_empty()) {
        if artist.trim().is_empty() {
            tracing::info!("Searching for Album ({})", a.trim());
        } else {
            tracing::info!(
                "Searching for Artist + Album ({} {})",
                artist.trim(),
                a.trim()
            );
        }
    } else {
        tracing::info!("Searching for Artist ({artist})");
    }
    let results = search_album(client, artist, album, timeout_secs).await?;
    if !results.is_empty() {
        if tier_has_usable_results(&results, filters, library_track_count, album) {
            return Ok(SearchOutcome { results });
        }
        fallback = Some(results);
    }

    // Tier 1b: lowercase fallback (when original casing was not usable)
    if let Some(album_name) = album {
        if !album_name.trim().is_empty() && !artist.trim().is_empty() {
            let artist_lower = artist.to_lowercase();
            let album_lower = album_name.to_lowercase();
            if artist_lower != artist || album_lower != album_name {
                tracing::info!(
                    "Searching for Artist + Album lowercase ({} {})",
                    artist_lower.trim(),
                    album_lower.trim()
                );
                let lower_results =
                    search_album(client, &artist_lower, Some(&album_lower), timeout_secs).await?;
                if !lower_results.is_empty() {
                    if tier_has_usable_results(&lower_results, filters, library_track_count, album)
                    {
                        return Ok(SearchOutcome {
                            results: lower_results,
                        });
                    }
                    if fallback.is_none() {
                        fallback = Some(lower_results);
                    }
                }
            }
        }
    }

    // Tier 1c: punctuation-normalised fallback (when the casing variants
    // were not usable). Peers often list names with different punctuation.
    if let Some(album_name) = album {
        if !album_name.trim().is_empty() && !artist.trim().is_empty() {
            let artist_norm = normalize_search_term(artist);
            let album_norm = normalize_search_term(album_name);
            let artist_collapsed = artist.split_whitespace().collect::<Vec<_>>().join(" ");
            let album_collapsed = album_name.split_whitespace().collect::<Vec<_>>().join(" ");
            if !artist_norm.is_empty()
                && !album_norm.is_empty()
                && (artist_norm != artist_collapsed || album_norm != album_collapsed)
            {
                tracing::info!(
                    "Searching for Artist + Album punctuation-normalised ({} {})",
                    artist_norm,
                    album_norm
                );
                let norm_results =
                    search_album(client, &artist_norm, Some(&album_norm), timeout_secs).await?;
                if !norm_results.is_empty() {
                    if tier_has_usable_results(&norm_results, filters, library_track_count, album)
                    {
                        return Ok(SearchOutcome {
                            results: norm_results,
                        });
                    }
                    if fallback.is_none() {
                        fallback = Some(norm_results);
                    }
                }
            }
        }
    }

    // Tier 2: album-only search (when the casing and punctuation variants
    // were not usable)
    if let Some(album_name) = album {
        if !album_name.trim().is_empty() {
            tracing::info!("Searching for Album ({})", album_name.trim());
            let album_results = search_album(client, "", Some(album_name), timeout_secs).await?;
            let mut artist_matches: Vec<SearchResult> = album_results
                .into_iter()
                .filter(|r| r.files.iter().any(|f| path_matches_artist(&f.name, artist)))
                .collect();
            for result in &mut artist_matches {
                result
                    .files
                    .retain(|f| path_matches_artist(&f.name, artist));
            }
            if !artist_matches.is_empty() {
                if tier_has_usable_results(&artist_matches, filters, library_track_count, album) {
                    return Ok(SearchOutcome {
                        results: artist_matches,
                    });
                }
                if fallback.is_none() {
                    fallback = Some(artist_matches);
                }
            }
        }
    }

    // No tier yielded a usable result set: return the first non-empty tier's
    // results (today's behaviour) or empty if every tier was empty.
    Ok(SearchOutcome {
        results: fallback.unwrap_or_default(),
    })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test cascade_continues_when_primary_only_mp3 cascade_returns_first_tier`
Expected: PASS — `2 passed; 0 failed`.

Run: `cargo test -p seakarr search::`
Expected: PASS — all `search::` tests green (the existing `.flac` mock files pass the probe, so prior expectations hold).

- [ ] **Step 5: Update the doc comment**

Replace the doc comment above `search_album_with_fallback` with:

```rust
/// Search for an album by artist + album name, stopping at the first tier
/// whose results survive the full filter pipeline
/// ([`crate::filter::filter_results`]).
///
/// Tier 1a runs the primary "Artist Album" search (original casing). Tier 1b
/// retries lowercased (Soulseek returns different result sets per casing, and
/// lowercase tends to be lower quality, so original casing is preferred);
/// skipped when the artist is empty or the query is already lowercase.
/// Tier 1c retries with punctuation normalised via [`normalize_search_term`]
/// (case preserved; skipped when normalisation changes nothing). Tier 2 falls
/// back to an album-name-only search (artist `""`), keeping only results whose
/// file paths match the artist via [`path_matches_artist`].
///
/// A tier is accepted only when it yields at least one result that passes the
/// filters (extension, bitrate, slots, min tracks, contiguity, album gate).
/// When a tier returns non-empty but unusable results, the cascade continues
/// to the next tier. If no tier yields usable results, the first non-empty
/// tier's results are returned so the caller's rejection summary and
/// title-search fallback still have data. Only when every tier is empty is an
/// empty outcome returned.
```

- [ ] **Step 6: Run full verification and commit**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all three pass; tests `0 failed`.

```bash
git add src/search.rs
git commit -m "feat: continue search cascade until a tier survives filtering"
```

---

## Self-Review

**Spec coverage:**
- Signature gains `filters` + `library_track_count` → Task 1 Step 1. ✅
- Per-tier probe with `filter_results(&output, filters, library_track_count, Some(album))` → Task 2 Step 3 (`tier_has_usable_results` + gated returns). ✅
- Probe non-empty → return tier; empty → remember fallback and continue → Task 2 Step 3. ✅
- No tier passes → first non-empty tier returned; all empty → empty → Task 2 Step 3 (`fallback.unwrap_or_default()`). ✅
- Runner one-line change → Task 1 Step 3. ✅
- Spec's new test (a) later tier wins and (b) all-junk returns first tier → Task 2 Step 1. Spec's test (c) "flac first → single query" is already covered by the existing `test_no_fallback_when_primary_non_empty` (`.flac` mock passes the probe), so no redundant test is added. ✅

**Placeholder scan:** no TBD/TODO; every code step includes complete code; every run step has the exact command and expected outcome.

**Type consistency:** `tier_has_usable_results(&[SearchResult], &FilterConfig, Option<usize>, Option<&str>) -> bool` is the single helper signature used consistently across all four tiers; `test_filters() -> FilterConfig` is defined in Task 1 and reused in Task 2; `library_track_count: Option<usize>` matches the runner's `process_album` parameter type.
