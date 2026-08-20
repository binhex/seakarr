# Seakarr — Search Fallback Overhaul: Design Spec

**Date:** 2026-08-20
**Status:** Draft
**Context:** The current search fallback has critical bugs. The primary `"Artist Album"` search
is blocked for certain artists (Prince, etc.). The title-search fallback uses generic track
names (`"CD Track 1"`) that produce garbage queries and match unrelated albums (e.g. downloading
`serial experiments lain` tracks for a Prince album). This overhaul adds an album-only fallback
tier and filters out generic track names.

---

## 1. Problem

The current fallback hierarchy is:

1. **Primary:** `"Artist Album"` — blocked for certain artists
2. **Title-search:** search by first library track name — fails when track names are generic

**Critical bug observed:** For "Prince — The Very Best Of Prince", library tracks are named
`"CD Track 1"`, `"CD Track 2"`, etc. The title-search fallback queried `"track 1"`, which matched
`CD Track 10..45.flac` from a completely unrelated `serial experiments lain BOOTLEG` share —
downloading 36 wrong tracks.

**Secondary bug:** Generic/embedded-metadata queries like `"musicology 2004 01 musicology"` (year
+ track number retained from filename) don't match well → 0 results.

## 2. Solution

Add an **album-only fallback tier** between primary and title-search, and **filter out generic
track names** from the title-search fallback:

1. **Primary:** `"Artist Album"` — as today
2. **Album-only:** `"Album"` only — filter by artist match using `path_matches_artist()`, no
   track-name verification
3. **Title-search:** search by track name — filter out generic names (`CD Track N`, `Track N`,
   `Untitled`, etc.), skip entirely if all names are generic

## 3. Architecture

```
runner.process_album
  └─ search_album_with_fallback(artist, album, timeout)
       ├─ Tier 1: search("Artist Album") ── non-empty → return results
       └─ Tier 2: search("Album") + artist verification
            ├─ search_album("", album)  // album name only
            ├─ Filter: path_matches_artist(artist) on each result
            ├─ Return matching results (no track-name verification)

  └─ (if search_album_with_fallback returns empty)
       └─ Tier 3: title-search fallback (existing, with improvements)
            ├─ Filter out generic track names (CD Track N, Track N, etc.)
            ├─ If no non-generic names remain → skip entirely
            ├─ search_by_title(client, non_generic_filenames, artist, ...)
            └─ Return matching results
```

**Key changes:**
- `search_album_with_fallback` gains the album-only tier (currently a no-op wrapper)
- `path_matches_artist()` is moved from `#[cfg(test)]` to production code
- `search_by_title` gains generic name filtering
- Runner's title-search fallback unchanged except it passes filtered filenames

## 4. Album-Only Tier in `search_album_with_fallback`

### Current Signature

```rust
pub async fn search_album_with_fallback(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    timeout_secs: u64,
) -> Result<SearchOutcome>
```

### New Behavior

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

### Key Points

- `path_matches_artist` is moved from `#[cfg(test)]` to production (remove the `#[cfg(test)]`
  attribute)
- The album-only search uses `""` for artist (album name only query)
- Results are filtered by artist match on file paths
- No track-name verification — we trust the album name + artist match
- Quality filters (bitrate, extension, slots) are applied later by `filter_results` in the runner

## 5. Generic Name Filtering

### New Function: `is_generic_track_name`

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

### Patterns Filtered

- `CD Track N`, `CD TrackN` (with or without space)
- `Track N`, `TrackN`
- `Untitled`, `Unknown`
- `Audio N`, `Recording N`
- Bare numbers (`01`, `1`, `42`)

### Integration in `search_by_title`

```rust
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
// Use non_generic filenames for the rest of the function
let clean_titles: Vec<String> = non_generic
    .iter()
    .map(|filename| clean_track_title(filename))
    .filter(|t| !t.is_empty())
    .collect();
```

## 6. Runner Changes

### Current Flow

```
search_album_with_fallback → filter_results → (if empty) title-search fallback
```

### New Flow

```
search_album_with_fallback (now includes album-only tier)
  → filter_results (with album gate for primary, no album gate for album-only)
  → (if empty) title-search fallback (with generic name filtering)
```

### Key Changes

1. **Logging for album-only tier:** Add a log line when the album-only tier fires:
   ```rust
   tracing::info!("{artist} — {album_name}: primary search empty, trying album-only search");
   ```

2. **Title-search fallback with filtered filenames:** Before calling `search_by_title`, filter
   out generic filenames:
   ```rust
   let non_generic: Vec<String> = lib_filenames
       .iter()
       .filter(|f| !search::is_generic_track_name(f))
       .cloned()
       .collect();
   if non_generic.is_empty() {
       tracing::info!("{artist} — {album_name}: all track names are generic, skipping title-search fallback");
   } else {
       match search::search_by_title(client, &non_generic, artist, ...) {
           // ...
       }
   }
   ```

3. **Filter results for album-only tier:** The album-only tier returns results that already
   passed `path_matches_artist`, but they still need quality filters. The runner applies
   `filter_results` with `album: None` (no album gate) since the album-only tier already
   verified the artist.

## 7. Testing Strategy

### Unit Tests (`src/search.rs`)

- `test_album_only_fallback_when_primary_empty` — primary returns nothing, album-only returns
  results matching artist → verify results returned
- `test_album_only_fallback_filters_by_artist` — album-only returns results with wrong artist
  → verify filtered out
- `test_album_only_fallback_skips_when_album_empty` — album is `None` or empty → verify no
  album-only search
- `test_is_generic_track_name` — verify filtering of CD Track N, Track N, Untitled, Unknown,
  Audio N, bare numbers
- `test_is_generic_track_name_non_generic` — verify real track names pass through (e.g.
  "Tomorrow Comes Today", "Musicology")
- `test_search_by_title_skips_generic_names` — library has only generic names → verify returns
  empty
- `test_search_by_title_uses_non_generic_names` — library has mix of generic and real names →
  verify only real names used

### Unit Tests (`src/runner.rs`)

- `test_album_only_fallback_fires_when_primary_empty` — primary search returns nothing,
  album-only returns results → verify download succeeds
- `test_album_only_fallback_does_not_fire_when_primary_has_results` — primary returns results
  → verify no album-only search
- `test_title_search_skips_when_all_tracks_generic` — all library tracks are generic names →
  verify title-search fallback skipped

### Integration Test

- `test_full_fallback_hierarchy` — primary blocked, album-only finds match → verify correct
  tier used

## 8. Files to Modify

| File | Change |
|------|--------|
| `src/search.rs` | Add `is_generic_track_name()` function; move `path_matches_artist()` from `#[cfg(test)]` to production; modify `search_album_with_fallback()` to add album-only tier; modify `search_by_title()` to filter generic names |
| `src/runner.rs` | Add logging for album-only tier; filter generic filenames before title-search fallback |

## 9. Dependencies

- `regex` — already in Cargo.toml (used by `clean_track_title`)

## 10. Config Changes

None. The search overhaul is transparent — no new config keys. The existing `search_title_match`
threshold is reused for the title-search fallback.

## 11. Out of Scope

- MusicBrainz integration (cancelled earlier)
- Configurable generic name patterns (hardcoded list)
- Album-only fallback when primary results fail filters (only when primary returns nothing)
- Track-name verification for album-only tier (skipped per user decision)
- Changes to the primary search query construction
