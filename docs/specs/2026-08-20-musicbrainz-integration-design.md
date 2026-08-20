# Seakarr — MusicBrainz Integration for Title-Search Fallback: Design Spec

**Date:** 2026-08-20
**Status:** Draft
**Context:** Soulseek blocks certain artists (e.g. Prince) from artist+album searches. The
title-search fallback (tier 3) searches by track name instead, but currently derives track names
from library filenames. If the library doesn't have the album, there are no filenames to use.
MusicBrainz provides clean track names externally, making the fallback work regardless of library
state.

---

## 1. Problem

The current title-search fallback (tier 3) gets track names from library filenames via
`clean_track_title()`. This has two problems:

1. **No library = no fallback.** If the library doesn't have the album, there are no filenames
   to derive track names from, and the fallback cannot fire.
2. **Messy filenames.** Library filenames vary wildly (`Prince_-_Musicology_2004_01_Musicology.mp3`
   vs `01 Musicology.flac`). Cleaning helps but is lossy.

MusicBrainz provides authoritative track listings for any album, making the fallback reliable
regardless of library state.

## 2. Solution

Replace the filename-based track-name source with MusicBrainz. When the title-search fallback
fires:

1. Query MusicBrainz for the artist+album track listing.
2. Search Soulseek for the first track name.
3. Verify that the peer's folder contains N% of the album's tracks (using existing
   `search_title_match` threshold).
4. If MusicBrainz fails (network error, not found, rate limited), fall back to library filenames.

## 3. Architecture

```
runner.process_album
  └─ search_album_with_fallback
       ├─ Tier 1: search("Artist Album") ── non-empty → done
       ├─ Tier 2: search("Album") + artist path filter ── non-empty → done
       └─ Tier 3: title-search fallback (when both above return empty)
            ├─ musicbrainz::get_track_names(db, artist, album)
            │    ├─ Check SQLite cache → hit? return cached tracks
            │    └─ Cache miss → HTTP GET MusicBrainz /release API
            │         ├─ Score releases by track-count similarity to library
            │         ├─ Parse best release → extract track names
            │         ├─ Cache result in SQLite
            │         └─ Return track names (or error → fall back to filenames)
            ├─ search_by_title(client, track_names, artist, timeout, threshold)
            │    ├─ Clean track names with clean_track_title()
            │    ├─ Search Soulseek for first cleaned track name (artist stripped)
            │    ├─ For each result: verify N% of tracks present in peer's folder
            │    └─ Return matching results
            └─ (if MusicBrainz failed) fall back to library filenames
```

**Key change:** `search_by_title` becomes source-agnostic — it accepts `track_names: &[String]`
instead of deriving them from filenames. The runner decides whether those names come from
MusicBrainz or library filenames.

## 4. `src/musicbrainz.rs` — New Module

### Public API

```rust
/// Fetch track names for an album from MusicBrainz.
/// Returns Ok(track_names) on success, Err on network/parse failure.
/// Cached in SQLite to avoid rate-limit issues.
pub async fn get_track_names(
    db: &Database,
    artist: &str,
    album: &str,
    library_track_count: Option<usize>,  // for scoring releases
) -> Result<Vec<String>>
```

### Constants

- `MBZ_USER_AGENT`: `"seakarr/<version> (https://github.com/binhex/seakarr)"`
- `MBZ_API_BASE`: `"https://musicbrainz.org/ws/2"`
- `MBZ_CACHE_TTL_DAYS`: `30`

### HTTP Client

- `reqwest::Client` with User-Agent header, constructed via `OnceLock` (reused across calls).
- Query: `GET /release?query=artist:{artist} AND release:{album}&fmt=json&limit=5`
- Parse: extract `releases[].media[0].track-list[].title` from each release.

### Release Scoring

When multiple releases are returned, score each by track-count similarity to the library's
track count (if available):

```
score = 1.0 - abs(release_track_count - library_track_count) / max(release_track_count, library_track_count)
```

- If the library has 12 tracks and MusicBrainz returns a 12-track release (score 1.0) and an
  18-track deluxe release (score 0.33), pick the 12-track release.
- If `library_track_count` is `None` (library doesn't have the album), use the first release.

### Track Name Cleaning

Apply `clean_track_title()` to each MusicBrainz track name before returning. MusicBrainz names
are already clean text (e.g. "Musicology"), so cleaning is minimal (lowercase, collapse
whitespace). This ensures downstream matching uses the same normalization as peer filenames.

### Error Handling

- Network errors → `Err` (caller falls back to filenames)
- No results found → `Ok(vec![])` (empty, not an error — album genuinely isn't in MusicBrainz)
- Rate limit (HTTP 429) → `Err` (caller falls back to filenames)
- Cache hit → return cached tracks, skip HTTP
- Empty result → NOT cached (so it retries next time)

## 5. SQLite Cache Schema

```sql
CREATE TABLE IF NOT EXISTS mbz_cache (
    artist      TEXT NOT NULL,
    album       TEXT NOT NULL,
    tracks_json TEXT NOT NULL,   -- JSON array of track names
    fetched_at  TEXT NOT NULL,   -- ISO 8601 timestamp
    PRIMARY KEY (artist, album)
);
```

### Operations

- `get(artist, album) → Option<Vec<String>>`: query by artist+album, check if `fetched_at` is
  within TTL (30 days). Return `Some(tracks)` if fresh, `None` if expired or missing.
- `put(artist, album, tracks)`: INSERT OR REPLACE with current timestamp.

### Migration

Add table creation to existing schema initialization in `db.rs`. No migration needed — new table,
not a column change.

## 6. Refactoring `search_by_title`

### Current Signature

```rust
pub async fn search_by_title(
    client: &dyn SoulseekClient,
    library_filenames: &[String],
    artist: &str,
    timeout_secs: u64,
    match_threshold_pct: u32,
) -> Result<Vec<SearchResult>>
```

### New Signature

```rust
pub async fn search_by_title(
    client: &dyn SoulseekClient,
    track_names: &[String],  // pre-cleaned track names (from MBZ or filenames)
    artist: &str,
    timeout_secs: u64,
    match_threshold_pct: u32,
) -> Result<Vec<SearchResult>>
```

### What Changes

- `library_filenames` parameter becomes `track_names` — already cleaned track names.
- Remove the internal `clean_track_title()` loop (caller provides cleaned names).
- The `fallback_track_query()` call stays (strips artist from the first track name).
- The threshold verification stays (check N% of tracks present in peer's folder).
- `clean_track_title()` is still used to normalize **peer filenames** from Soulseek results
  for matching.

### Caller Change in `runner.rs`

```rust
let track_names = match musicbrainz::get_track_names(db, artist, album_name, Some(library_track_count)).await {
    Ok(names) if !names.is_empty() => names,
    _ => {
        // Fallback: derive from library filenames
        lib_filenames.iter()
            .map(|f| search::clean_track_title(f))
            .filter(|t| !t.is_empty())
            .collect()
    }
};
if !track_names.is_empty() {
    match search::search_by_title(client, &track_names, artist, ...) {
        // ...
    }
}
```

## 7. Track Name Cleaning

The existing `clean_track_title()` handles both MusicBrainz track names and peer filenames:

### MusicBrainz Track Names (already clean)

Input: `"Musicology"` → Output: `"musicology"`
Input: `"Cinnamon Girl"` → Output: `"cinnamon girl"`

Cleaning is minimal: lowercase, collapse whitespace. MusicBrainz names don't have track numbers,
extensions, or brackets.

### Peer Filenames (messy)

Input: `"Prince - Musicology (2004) 01 Musicology.mp3"` → Output: `"prince musicology 2004 01 musicology"`
Input: `"01. Musicology.flac"` → Output: `"musicology"`

Cleaning strips extension, track number, brackets, normalizes unicode, removes punctuation.

### Matching Logic

After cleaning, the matching compares cleaned MusicBrainz names against cleaned peer filenames.
Both go through the same `clean_track_title()` pipeline, so normalization is consistent.

**Accepted risk:** A peer filename like `"Prince - Musicology (2004) 01 Musicology.mp3"` cleans
to `"prince musicology 2004 01 musicology"`, which contains the track name `"musicology"` as a
substring. The current exact-match approach in `search_by_title` would miss this. The matching
should use **substring containment** (cleaned peer filename contains the cleaned track name) rather
than exact equality.

## 8. Testing Strategy

### Unit Tests (`src/musicbrainz.rs`)

- `test_parse_track_names_from_release_json` — parse sample MusicBrainz JSON, verify track names
- `test_parse_empty_release_returns_empty` — no tracks → `Ok(vec![])`
- `test_parse_malformed_json_returns_error` — invalid JSON → `Err`
- `test_cache_hit_skips_http` — insert into SQLite, verify second call returns cached
- `test_cache_expired_triggers_refresh` — insert with old `fetched_at`, verify re-fetch
- `test_cache_empty_result_not_cached` — `Ok(vec![])` should NOT be cached
- `test_release_scoring_picks_best_match` — verify scoring selects the release with closest
  track count to the library

### Unit Tests (`src/search.rs`)

- Update existing `search_by_title` tests to pass pre-cleaned track names
- `test_search_by_title_with_musicbrainz_tracks` — pass MBZ-style track names, verify matching
- `test_search_by_title_empty_tracks_returns_empty` — empty track list → `Ok(vec![])`
- `test_search_by_title_substring_matching` — peer filename contains track name as substring

### Integration Tests (`src/runner.rs`)

- `test_musicbrainz_fallback_when_primary_empty` — mock MBZ HTTP (via `wiremock`), verify
  title-search fallback fires with MBZ track names
- `test_musicbrainz_failure_falls_back_to_filenames` — mock MBZ to return error, verify fallback
  uses library filenames
- `test_musicbrainz_cache_hit_no_http` — pre-populate SQLite cache, verify no HTTP call made

### Live Test

After implementation, perform a live test with Prince's "Musicology" album to verify the full
flow works end-to-end (manual mode search).

## 9. Files to Modify

| File | Change |
|------|--------|
| `src/musicbrainz.rs` | **NEW** — MusicBrainz API client, cache, parsing, release scoring |
| `src/lib.rs` | Add `mod musicbrainz` |
| `src/search.rs` | Refactor `search_by_title` to accept `track_names: &[String]`; add substring matching |
| `src/runner.rs` | Wire MusicBrainz into title-search fallback, with filename fallback on error |
| `src/db.rs` | Add `mbz_cache` table creation to schema init |
| `Cargo.toml` | Add `serde_json` dependency |

## 10. Dependencies

- `reqwest` — already in Cargo.toml (used by notifier)
- `serde_json` — **need to add** (for parsing MusicBrainz JSON responses)
- `rusqlite` — already used (for cache table)

## 11. Config Changes

None. The MusicBrainz integration is transparent — no new config keys. The existing
`search_title_match` threshold is reused. The cache TTL is hardcoded (30 days).

## 12. Out of Scope

- MBID-based lookup (user chose name-only search)
- Configurable User-Agent (hardcoded)
- Configurable cache TTL (hardcoded 30 days)
- `--refresh-mbz` CLI flag (can add later)
- Cover art from MusicBrainz/Cover Art Archive
- MusicBrainz for primary search (only used in title-search fallback)
- Fuzzy/similarity track matching (exact/substring match only)
