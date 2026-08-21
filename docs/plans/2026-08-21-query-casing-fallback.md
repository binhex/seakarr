# Query Casing Fallback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a lowercase casing fallback to `search_album_with_fallback` so that when the original-casing search returns zero raw results, the lowercase variant is tried before falling back to the album-only tier.

**Architecture:** `search_album_with_fallback` gains a new Tier 1b between the original-casing search (Tier 1a) and the album-only tier (Tier 2). The lowercase fallback only fires when Tier 1a returned zero raw results. `search_album` stays unchanged.

**Tech Stack:** Rust, `unicode-normalization` (already used)

---

## File Structure

| File | Responsibility | Changes |
|------|---------------|---------|
| `src/search.rs` | Search logic, fallback tiers | Modify `search_album_with_fallback()` to add lowercase casing fallback |

---

### Task 1: Add Lowercase Casing Fallback to `search_album_with_fallback`

**Files:**
- Modify: `src/search.rs:59-95` (`search_album_with_fallback` function)

**Context:** When the primary `"Artist Album"` search returns nothing, try the lowercase variant before falling back to the album-only tier. This handles the case where Soulseek returns different (better) results for title-case queries, but also returns results for lowercase queries when title-case finds nothing.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/search.rs`:

```rust
#[tokio::test]
async fn test_lowercase_fallback_when_primary_empty() {
    let client = MockClient::new();
    // Primary "Prince Musicology" has no map entry -> empty (blocked artist).
    // Lowercase "prince musicology" returns results.
    client.search_results_by_query.lock().unwrap().insert(
        "prince musicology".into(),
        vec![SearchResult {
            username: "peer1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                "Prince/Musicology/01 - Musicology.flac",
                900,
                30_000_000,
            )],
        }],
    );

    let outcome = search_album_with_fallback(&client, "Prince", Some("Musicology"), 15)
        .await
        .unwrap();
    assert_eq!(outcome.results.len(), 1);
    // Both tiers ran: original casing + lowercase fallback.
    let queries = client.search_queries.lock().unwrap().clone();
    assert_eq!(
        queries,
        vec![
            "Prince Musicology".to_string(),
            "prince musicology".to_string()
        ]
    );
}

#[tokio::test]
async fn test_lowercase_fallback_skipped_when_primary_has_results() {
    let client = MockClient::new();
    // Primary "Prince Musicology" returns results (even if they fail filters).
    // Lowercase fallback must NOT be attempted.
    client.search_results_by_query.lock().unwrap().insert(
        "Prince Musicology".into(),
        vec![SearchResult {
            username: "peer1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                "Prince/Musicology/01 - Musicology.flac",
                900,
                30_000_000,
            )],
        }],
    );

    let outcome = search_album_with_fallback(&client, "Prince", Some("Musicology"), 15)
        .await
        .unwrap();
    assert_eq!(outcome.results.len(), 1);
    // Only the original casing query was issued — no lowercase fallback.
    let queries = client.search_queries.lock().unwrap().clone();
    assert_eq!(queries, vec!["Prince Musicology".to_string()]);
}

#[tokio::test]
async fn test_album_only_tier_after_both_casings_fail() {
    let client = MockClient::new();
    // Both casing variants return nothing. Album-only "Musicology" returns
    // a result whose path matches "Prince".
    client.search_results_by_query.lock().unwrap().insert(
        "Musicology".into(),
        vec![SearchResult {
            username: "peer1".into(),
            speed: 500,
            slots: 1,
            files: vec![make_file(
                "Prince/Musicology/01 - Musicology.flac",
                900,
                30_000_000,
            )],
        }],
    );

    let outcome = search_album_with_fallback(&client, "Prince", Some("Musicology"), 15)
        .await
        .unwrap();
    assert_eq!(outcome.results.len(), 1);
    // Three queries ran: original casing, lowercase, album-only.
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

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib search::tests::test_lowercase_fallback`
Expected: FAIL (current implementation doesn't have lowercase fallback)

- [ ] **Step 3: Implement lowercase casing fallback**

In `src/search.rs`, modify `search_album_with_fallback` (lines 59-95). Add Tier 1b between the original-casing search and the album-only tier:

```rust
pub async fn search_album_with_fallback(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    timeout_secs: u64,
) -> Result<SearchOutcome> {
    // Tier 1a: primary "Artist Album" search (original casing)
    let results = search_album(client, artist, album, timeout_secs).await?;
    if !results.is_empty() {
        return Ok(SearchOutcome { results });
    }

    // Tier 1b: lowercase fallback (when original casing returned nothing)
    // Soulseek returns different result sets for different query casing;
    // lowercase queries tend to return lower-quality results (MP3s), so
    // we try the original casing first and only fall back to lowercase
    // when it returned zero raw results.
    if let Some(album_name) = album {
        if !album_name.trim().is_empty() {
            let artist_lower = artist.to_lowercase();
            let album_lower = album_name.to_lowercase();
            let lower_results =
                search_album(client, &artist_lower, Some(&album_lower), timeout_secs).await?;
            if !lower_results.is_empty() {
                return Ok(SearchOutcome {
                    results: lower_results,
                });
            }
        }
    }

    // Tier 2: album-only search (when both casing variants returned nothing)
    if let Some(album_name) = album {
        if !album_name.trim().is_empty() {
            let album_results = search_album(client, "", Some(album_name), timeout_secs).await?;
            // Filter by artist match using existing path_matches_artist,
            // and prune each result's files to only artist-matching ones
            // so we don't download tracks from a different artist's
            // same-named album directory.
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
                return Ok(SearchOutcome {
                    results: artist_matches,
                });
            }
        }
    }

    // All tiers returned nothing
    Ok(SearchOutcome { results: vec![] })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib search::tests::test_lowercase_fallback`
Expected: PASS

- [ ] **Step 5: Run full test suite**

Run: `cargo test`
Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add src/search.rs
git commit -m "feat: add lowercase casing fallback to search_album_with_fallback"
```

---

### Task 2: Final Verification and Cleanup

**Files:**
- None (verification only)

- [ ] **Step 1: Run full test suite**

Run: `cargo test`
Expected: All tests pass (should be 460+ tests)

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
