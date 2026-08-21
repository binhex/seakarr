# Seakarr — Query Casing Fallback: Design Spec

**Date:** 2026-08-21
**Status:** Draft
**Context:** Soulseek returns different result sets for different query casing. Lowercase queries
tend to return lower-quality results (mostly MP3s), while title-case queries return better results
(FLACs). When a user provides a lowercase album name (e.g. `--album "the singles collection"`),
the search returns results that all fail quality filters (FLAC-only, free slots, etc.), even though
the same album with title-case casing (`--album "The Singles Collection"`) succeeds.

---

## 1. Problem

The user ran the same manual search twice with different casing:

- `--album "the singles collection"` → 422 files from 29 users, **0 passed filters** (190 rejected for format, mostly MP3)
- `--album "The Singles Collection"` → 335 files from 26 users, **1 user passed filters** (FLAC files with free slots)

The code already handles case-insensitivity in the filter/matching pipeline (`word_tokens` lowercases
everything, `path_matches_artist` lowercases). The issue is that **Soulseek itself returns different
result sets for different query casing** — lowercase queries tend to return lower-quality results.

## 2. Solution

Add a **lowercase casing fallback** to `search_album_with_fallback`. Try the original casing first;
if it returns zero raw results, try the lowercase variant. This gives the best of both worlds:
title-case queries (better quality) are preferred, but lowercase queries (broader results) are
available as a fallback.

## 3. Architecture

```
search_album_with_fallback(artist, album, timeout)
  ├─ Tier 1a: search("{artist} {album}") — original casing
  ├─ Tier 1b: search("{artist_lower} {album_lower}") — lowercase fallback
  │   (only fires if Tier 1a returned zero raw results)
  ├─ Tier 2: search("{album}") + artist verification
  │   (only fires if both Tier 1a and 1b returned nothing)
  └─ Return empty if all tiers failed
```

**Key behavior:** The lowercase fallback only fires when the original-casing search returned
**zero raw results** (not when results exist but fail filters). If title-case finds results
(even if they fail filters), we don't try lowercase.

## 4. Implementation

### Modified Function: `search_album_with_fallback`

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
            let lower_results = search_album(client, &artist_lower, Some(&album_lower), timeout_secs).await?;
            if !lower_results.is_empty() {
                return Ok(SearchOutcome { results: lower_results });
            }
        }
    }

    // Tier 2: album-only search (when both casing variants returned nothing)
    if let Some(album_name) = album {
        if !album_name.trim().is_empty() {
            let album_results = search_album(client, "", Some(album_name), timeout_secs).await?;
            let mut artist_matches: Vec<SearchResult> = album_results
                .into_iter()
                .filter(|r| r.files.iter().any(|f| path_matches_artist(&f.name, artist)))
                .collect();
            for result in &mut artist_matches {
                result.files.retain(|f| path_matches_artist(&f.name, artist));
            }
            if !artist_matches.is_empty() {
                return Ok(SearchOutcome { results: artist_matches });
            }
        }
    }

    Ok(SearchOutcome { results: vec![] })
}
```

### Key Points

- `artist.to_lowercase()` and `album_name.to_lowercase()` normalize the query
- The lowercase fallback only fires when the original query returned zero raw results
- The album-only tier (Tier 2) stays unchanged — it already uses the original casing for artist verification
- `search_album` stays unchanged (simple query primitive)

## 5. Testing Strategy

### Unit Tests (`src/search.rs`)

- `test_lowercase_fallback_when_primary_empty` — primary "Prince Musicology" returns nothing, lowercase "prince musicology" returns results → verify results returned
- `test_lowercase_fallback_skipped_when_primary_has_results` — primary returns results (even if they fail filters) → verify lowercase fallback NOT attempted
- `test_lowercase_fallback_skipped_when_primary_returns_raw_results` — primary returns raw results that all fail filters → verify lowercase fallback NOT attempted (raw results ≠ empty)
- `test_album_only_tier_after_both_casings_fail` — both casing variants return nothing → verify album-only tier fires

### Integration Test (`src/runner.rs`)

- `test_lowercase_fallback_fires_when_primary_empty` — primary returns nothing, lowercase returns results → verify download succeeds

## 6. Files to Modify

| File | Change |
|------|--------|
| `src/search.rs` | Modify `search_album_with_fallback()` to add lowercase casing fallback |

## 7. Config Changes

None. The casing fallback is transparent — no new config keys.

## 8. Out of Scope

- Changing the album-only tier casing (it already uses the original casing for artist verification)
- Configurable casing strategy (hardcoded: title-case first, lowercase fallback)
- Changing `search_album` itself (it stays a simple query primitive)
- Changing `filter_results` or `path_matches_artist` (already case-insensitive)
