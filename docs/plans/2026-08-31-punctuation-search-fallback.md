# Punctuation-Normalised Search Fallback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a case-preserving, punctuation-normalised artist+album search tier so albums whose punctuation differs from the peer's listing (e.g. `S.P.Y.` vs `SPY`, `Guns & Roses` vs `Guns and Roses`) are still found.

**Architecture:** A pure `normalize_search_term(&str) -> String` function folds accents and normalises punctuation, and a new Tier 1c in `search_album_with_fallback` issues a single `"{artist_norm} {album_norm}"` query between the lowercase tier (1b) and the album-only tier (2). Case is preserved; the tier is skipped when normalisation is a no-op.

**Tech Stack:** Rust (async/`tokio`), `regex` and `unicode_normalization` (both already used in `src/search.rs`), `tracing` for the log line.

---

## File Structure

| File | Responsibility | Change |
|------|----------------|--------|
| `src/search.rs` | Search cascade + normalization helpers | Add `normalize_search_term` fn; insert Tier 1c in `search_album_with_fallback`; update doc comments; add unit + tier tests |

Only one file changes. The new function is a sibling of the existing `clean_track_title` normalization helper and lives next to its only caller (`search_album_with_fallback`).

---

### Task 1: Add `normalize_search_term` (TDD)

**Files:**
- Modify: `src/search.rs` (insert new function; no existing code changes)
- Test: `src/search.rs` (append tests to the `#[cfg(test)] mod tests` block)

- [ ] **Step 1: Write the failing unit tests**

Append these tests inside `mod tests` (after the existing `// ── clean_track_title ──` section):

```rust
    // ── normalize_search_term ──

    #[test]
    fn test_normalize_search_term_joins_periods() {
        assert_eq!(normalize_search_term("S.P.Y."), "SPY");
    }

    #[test]
    fn test_normalize_search_term_full_album() {
        assert_eq!(
            normalize_search_term("S.P.Y. - In The Skys"),
            "SPY In The Skys"
        );
    }

    #[test]
    fn test_normalize_search_term_ampersand_to_and() {
        assert_eq!(normalize_search_term("Guns & Roses"), "Guns and Roses");
    }

    #[test]
    fn test_normalize_search_term_separators() {
        assert_eq!(normalize_search_term("AC-DC"), "AC DC");
        assert_eq!(normalize_search_term("AC/DC"), "AC DC");
    }

    #[test]
    fn test_normalize_search_term_brackets_and_underscore() {
        assert_eq!(
            normalize_search_term("In_The-Skys (Deluxe)"),
            "In The Skys Deluxe"
        );
    }

    #[test]
    fn test_normalize_search_term_apostrophe_joins() {
        assert_eq!(normalize_search_term("D'Angelo"), "DAngelo");
    }

    #[test]
    fn test_normalize_search_term_accent_fold() {
        assert_eq!(normalize_search_term("Tiësto"), "Tiesto");
        assert_eq!(normalize_search_term("Café"), "Cafe");
    }

    #[test]
    fn test_normalize_search_term_preserves_case() {
        assert_eq!(
            normalize_search_term("Spy In The Skys"),
            "Spy In The Skys"
        );
    }

    #[test]
    fn test_normalize_search_term_no_change_passthrough() {
        assert_eq!(normalize_search_term("Musicology"), "Musicology");
    }

    #[test]
    fn test_normalize_search_term_collapses_whitespace() {
        assert_eq!(
            normalize_search_term("  Guns   &   Roses  "),
            "Guns and Roses"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test normalize_search_term`
Expected: FAIL — `error[E0425]: cannot find function 'normalize_search_term' in this scope` (all 10 tests fail to compile).

- [ ] **Step 3: Write the minimal implementation**

Insert this function immediately before `search_album_with_fallback` (after the `SearchOutcome` struct, around line 55 of `src/search.rs`). `unicode_normalization::UnicodeNormalization` is already imported at the top of the file, so `.nfkd()` is available.

```rust
/// Normalize an artist/album name for the punctuation-tolerant search tier.
///
/// Case is preserved (the lowercase tier covers that axis); accented
/// characters are folded to ASCII via NFKD ("Tiësto" -> "Tiesto"). The
/// ampersand and plus become the word "and"; tight punctuation (period,
/// comma, apostrophe, double-quote, backtick) is removed so letters join
/// ("S.P.Y." -> "SPY", "D'Angelo" -> "DAngelo"); separators (hyphen,
/// underscore, slash, backslash, and brackets) become spaces
/// ("In-The-Skys" -> "In The Skys", "AC/DC" -> "AC DC"). Whitespace is
/// collapsed and trimmed.
pub fn normalize_search_term(input: &str) -> String {
    let folded: String = input.nfkd().filter(|c| c.is_ascii()).collect();
    let mut out = String::with_capacity(folded.len() + 8);
    for c in folded.chars() {
        match c {
            '&' | '+' => out.push_str(" and "),
            '.' | ',' | '\'' | '"' | '`' => {}
            '-' | '_' | '/' | '\\' | '(' | ')' | '[' | ']' | '{' | '}' => out.push(' '),
            _ => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test normalize_search_term`
Expected: PASS — `test result: ok. 10 passed; 0 failed`.

- [ ] **Step 5: Commit**

```bash
git add src/search.rs
git commit -m "feat: add normalize_search_term for punctuation-tolerant search"
```

---

### Task 2: Insert Tier 1c into `search_album_with_fallback` (TDD)

**Files:**
- Modify: `src/search.rs` (insert Tier 1c between Tier 1b and Tier 2)
- Test: `src/search.rs` (append async tier tests)

- [ ] **Step 1: Write the failing tier tests**

Append these tests inside `mod tests` (after the existing `// ── lowercase casing fallback ──` section):

```rust
    // ── punctuation fallback ──

    #[tokio::test]
    async fn test_punctuation_fallback_fires_when_normalisation_changes() {
        let client = MockClient::new();
        // Tier 1a "S.P.Y. In The Skys" and Tier 1b "s.p.y. in the skys" have
        // no map entries -> empty. Tier 1c normalises to "SPY In The Skys".
        client.search_results_by_query.lock().unwrap().insert(
            "SPY In The Skys".into(),
            vec![SearchResult {
                username: "peer1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    "SPY/In The Skys/01 - Track.flac",
                    900,
                    30_000_000,
                )],
            }],
        );

        let outcome =
            search_album_with_fallback(&client, "S.P.Y.", Some("In The Skys"), 15)
                .await
                .unwrap();
        assert_eq!(outcome.results.len(), 1);
        assert_eq!(outcome.results[0].username, "peer1");
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
    async fn test_punctuation_fallback_skipped_when_no_punctuation() {
        let client = MockClient::new();
        // "Prince" + "Musicology" has no punctuation: Tier 1c normalisation
        // is a no-op and must be skipped. Tier 1a, 1b, and album-only run.
        let outcome = search_album_with_fallback(&client, "Prince", Some("Musicology"), 15)
            .await
            .unwrap();
        assert!(outcome.results.is_empty());
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

    #[tokio::test]
    async fn test_punctuation_fallback_ampersand() {
        let client = MockClient::new();
        client.search_results_by_query.lock().unwrap().insert(
            "Guns and Roses Appetite for Destruction".into(),
            vec![SearchResult {
                username: "peer1".into(),
                speed: 500,
                slots: 1,
                files: vec![make_file(
                    "Guns N Roses/Appetite for Destruction/01.flac",
                    900,
                    30_000_000,
                )],
            }],
        );

        let outcome =
            search_album_with_fallback(&client, "Guns & Roses", Some("Appetite for Destruction"), 15)
                .await
                .unwrap();
        assert_eq!(outcome.results.len(), 1);
        let queries = client.search_queries.lock().unwrap().clone();
        assert_eq!(
            queries,
            vec![
                "Guns & Roses Appetite for Destruction".to_string(),
                "guns & roses appetite for destruction".to_string(),
                "Guns and Roses Appetite for Destruction".to_string()
            ]
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test punctuation_fallback`
Expected: FAIL — the first test asserts 3 queries but only 2 are issued (Tier 1c not implemented yet), and `outcome.results.len()` is 0 instead of 1.

- [ ] **Step 3: Write the implementation**

Insert this block immediately after Tier 1b's closing brace and before the `// Tier 2: album-only search` comment (around line 120 of `src/search.rs`):

```rust
    // Tier 1c: punctuation-normalised fallback (when both casing variants
    // returned nothing). Peers often list names with different punctuation
    // ("S.P.Y." vs "SPY", "Guns & Roses" vs "Guns and Roses"), so a query
    // sent verbatim misses them. Case is preserved (the lowercase tier
    // already covered that axis); skip when normalisation changes nothing
    // (the query would be byte-identical to Tier 1a).
    if let Some(album_name) = album {
        if !album_name.trim().is_empty() && !artist.trim().is_empty() {
            let artist_norm = normalize_search_term(artist);
            let album_norm = normalize_search_term(album_name);
            if artist_norm != artist.trim() || album_norm != album_name.trim() {
                tracing::info!(
                    "Searching for Artist + Album punctuation-normalised ({} {})",
                    artist_norm,
                    album_norm
                );
                let norm_results =
                    search_album(client, &artist_norm, Some(&album_norm), timeout_secs).await?;
                if !norm_results.is_empty() {
                    return Ok(SearchOutcome {
                        results: norm_results,
                    });
                }
            }
        }
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test punctuation_fallback`
Expected: PASS — `test result: ok. 3 passed; 0 failed`.

Run the full search module to confirm no existing tier tests broke (their inputs have no punctuation, so Tier 1c is skipped and query counts are unchanged):

Run: `cargo test -p seakarr search::`
Expected: PASS — all `search::` tests green.

- [ ] **Step 5: Commit**

```bash
git add src/search.rs
git commit -m "feat: add punctuation-normalised search fallback tier"
```

---

### Task 3: Update doc comments + full verification

**Files:**
- Modify: `src/search.rs` (doc comment on `search_album_with_fallback`; inline comment on Tier 2)

- [ ] **Step 1: Update the `search_album_with_fallback` doc comment**

Replace the existing doc comment block (the paragraph above `pub async fn search_album_with_fallback`) with:

```rust
/// Search for an album by artist + album name.
///
/// Tier 1a runs the primary "Artist Album" search (original casing). If it
/// returns nothing, Tier 1b retries with the same query lowercased (Soulseek
/// returns different result sets per casing, and lowercase queries tend to be
/// lower quality, so original casing is always preferred). Tier 1b is skipped
/// when the artist is empty (Tier 2 handles album-only searches with artist
/// verification) or when the query is already lowercase (no new information).
/// If both casing variants return nothing, Tier 1c retries with punctuation
/// normalised via [`normalize_search_term`] (case preserved; skipped when
/// normalisation changes nothing). If all of those return nothing, Tier 2
/// falls back to an album-name-only search (artist `""`), keeping only results
/// whose file paths match the artist via [`path_matches_artist`]. If all
/// tiers come up empty, an empty outcome is returned.
```

- [ ] **Step 2: Update the Tier 2 inline comment**

Replace:

```rust
    // Tier 2: album-only search (when both casing variants returned nothing)
```

with:

```rust
    // Tier 2: album-only search (when the casing and punctuation variants
    // returned nothing)
```

- [ ] **Step 3: Run the full verification suite**

Run: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: all three pass. Tests: `496 passed` (the 13 new tests added in Tasks 1-2 bring the previous 496 up; the exact number is 509 — verify the reported count is 0 failed).

- [ ] **Step 4: Commit**

```bash
git add src/search.rs
git commit -m "docs: document punctuation-normalised tier in search_album_with_fallback"
```

---

## Self-Review

**Spec coverage:**
- `normalize_search_term` fn → Task 1. ✅
- Tier 1c inserted after 1b / before 2 → Task 2. ✅
- Skip when normalisation is a no-op → Task 2 Step 3 guard + Task 2 test `test_punctuation_fallback_skipped_when_no_punctuation`. ✅
- Case preserved, `&`/`+`→"and", join/separator classes, accent fold → Task 1 function + unit tests. ✅
- Descriptive log line → Task 2 Step 3 `tracing::info!`. ✅
- Doc comment update → Task 3. ✅

**Placeholder scan:** no TBD/TODO; every code step includes full code; every run step includes the exact command and expected outcome.

**Type consistency:** `normalize_search_term(&str) -> String` is the single signature used consistently in Tasks 1-3; tier tests reference `MockClient`, `SearchResult`, and `make_file` which already exist in the test module.
