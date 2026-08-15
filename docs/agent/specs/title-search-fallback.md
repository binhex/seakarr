# Title-Search Fallback (Third-Tier)

## Problem

When searching for albums like "Adele — 25", both the primary search ("Adele 25") and album-only fallback ("25") return zero results. The album exists on Soulseek, but peers share it under different naming conventions or the artist+album combination is banned by the server.

## Solution

Add a third-tier fallback that searches for the **first track's filename** from the library, then verifies other album tracks match before downloading. This is the **absolute last resort** — only triggered when both primary and album-only searches return zero results.

## Search Flow (3 tiers)

```
1. Primary: "Adele 25"
   ↓ (0 results)
2. Album-only: "25" → filter to keep only artist-matching paths
   ↓ (0 results)
3. Title search (LAST RESORT): "I Miss You" → verify library tracks match
```

## Track Name Cleaning

Aggressive normalize + strip:

- Remove leading track numbers: `03.`, `03 -`, `03-`, etc.
- Strip file extension: `.mp3`, `.flac`, etc.
- Remove brackets: `[...]`, `(...)`, `{...}`
- Normalize unicode (NFKD → ASCII)
- Lowercase
- Remove punctuation
- Trim whitespace

**Example:** `03. I Miss You.mp3` → `i miss you`

## Title Search Flow (Auto Mode Only)

1. Get library track filenames for the album (filesystem read on demand)
2. Extract clean title from first track (sorted alphabetically)
3. Search Soulseek for clean title only (e.g., "I Miss You")
4. For each search result:
   - Clean each file's basename
   - Check if any match a library track's clean title
   - Count matched library tracks
5. **Threshold**: ≥ `search_title_match` % of library tracks must match (default: 70%)
6. Apply existing filters (slots, bitrate, format, contiguity, min_tracks, peer_track_count)
7. Rank and download

## Config Changes

**`seakarr.yml` → `[search]` section:**

```yaml
search:
  timeout_secs: 30
  fallback_search: true
  search_title_match: 70  # minimum % of library tracks that must match title search results (0 = disabled)
```

## Files to Modify

| File | Change |
|------|--------|
| `src/config.rs` | Add `search_title_match: u32` to `SearchConfig` (default 70, 0 = disabled) |
| `src/search.rs` | Add `search_by_title()` function; add `clean_track_title()` helper; extend `SearchOutcome` with `used_title_search` |
| `src/runner.rs` | In `process_album`, add third-tier call after album-only fallback returns empty; pass library track filenames |
| `src/README.md` or `seakarr.yml` example | Document the new config entry |

## Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Track source | Read filesystem on demand | No schema change needed |
| Clean logic | Aggressive normalize + strip | Handles diverse naming conventions |
| Which track | First track (alphabetical) | Simple, deterministic |
| Search query | Title only | Broadest results, avoids artist name misspelling issues |
| Threshold | Configurable % (default 70) | Allows tuning; 0 disables |
| Mode scope | Auto mode only | Manual mode doesn't have library track context |
| Timeout | Same as existing | Consistent behavior |
| Filter order | All existing filters, then title match | Title search is last resort |
