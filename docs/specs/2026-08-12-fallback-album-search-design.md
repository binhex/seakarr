# Seakarr — Fallback Album Search for Banned Artist+Album Criteria: Design Spec

**Date:** 2026-08-12
**Status:** Approved
**Context:** Soulseek sometimes bans certain artist+album combined search criteria (observed: a
`Michael Jackson History` search returns zero results while an album-only `History` search returns
plenty). This spec adds a fallback: when the combined search returns zero results, search by album name
alone and accept results whose share-relative file paths match the artist. The fallback is opt-out and
changes nothing on the primary search path.

---

## 1. Behaviour

When `runner::process_album` processes an album (i.e. `album` is `Some`) and the primary
`"Artist Album"` search returns **zero raw results**, and `search.fallback_search` is enabled
(default `true`):

1. Run a second search with **only the album name**, using the same search timeout.
2. Keep only results where at least one file's share-relative path matches the artist (word-level
   match, Section 2).
3. If matches remain: log the fallback outcome, then feed the matching results into the **existing**
   `filter_results` → `rank_candidates` → `download_album` → organize → notify pipeline, unchanged.
4. If the fallback finds no matches (or is disabled, or `album` is `None`): current behaviour applies —
   log `No results for {artist} — {album}`, mark the album `skipped`, return `Ok`.

The fallback applies to **all modes** (auto/daemon, manual, batch) because they all route through
`process_album`. Zero-result albums retry on later daemon cycles (they are no longer blocked by
`is_album_processed`), so the fallback automatically fires on those retries with no extra machinery.

The primary search path is untouched: when the combined query returns any raw result, the fallback never
runs.

## 2. Artist matching

`path_matches_artist(path: &str, artist: &str) -> bool`

- Normalise the path: lowercase, replace `\` with `/`.
- Tokenise the artist into alphanumeric words (lowercased).
- Drop common articles (`the`, `a`, `an`) from the word list.
- **All** remaining words must appear as case-insensitive substrings somewhere in the normalised
  path.
- If no words remain (every word was a stop-word, e.g. artist `The The`), fall back to the full
  lowercased artist name as a substring match.

Rationale:

- Handles reordered names: `Jackson, Michael` contains both `jackson` and `michael`.
- Handles dropped articles: `The Beatles` shared as `Beatles` — `the` is a stop-word, `beatles` must
  appear.
- Handles punctuation: `AC/DC` becomes words `ac` and `dc`.
- Stop-words never block a match — only the distinctive words carry the decision.

Accepted risk: substring-per-word means `Prince` also matches a path containing `Princess`. Downstream
quality filters (extension, bitrate, exclude-words) still apply.

**Per-file filtering (2026-08-13 amendment):** a result passes only if at least one of its files'
paths matches the artist, AND within every kept result only the artist-matching files survive —
non-matching files are dropped inside `search_album_with_fallback` itself. This prevents mixed shares
(one matching file alongside other artists' tracks that match the broad album-only query) from
donating wrong-artist files to the download stage. Downloads therefore only ever take
artist-matching files. The whole album still comes from a single peer: `download_album`'s
per-candidate all-or-nothing behaviour is unchanged, and a peer switch happens only on a download
failure from the current candidate.

Accepted residual of this amendment: the filter matches the **artist only**, not artist+album. A
mixed share containing several albums by the SAME artist can still donate tracks from the wrong
album into the download set; and legitimate files whose share paths omit the artist are rejected
(precision over recall). Both are consequences of the word-level artist matching chosen in
Section 2.

## 3. Components

### `search.rs` (changes)

- `pub struct SearchOutcome { pub results: Vec<SearchResult>, pub used_fallback: bool, pub
  fallback_duration_ms: Option<u64> }` — the third field records the fallback search's own duration
  (`None` when the fallback did not run) so history rows can record per-search durations.
- `pub async fn search_album_with_fallback(client, artist, album: Option<&str>, timeout_secs,
  fallback_enabled: bool) -> Result<SearchOutcome>`:
  - Calls the existing `search_album` for the primary `"Artist Album"` query (reusing its
    deduplication). The album name is trimmed before use in the fallback query (padded tag metadata
    would otherwise defeat the album-only search), and a blank artist or blank album skips the
    fallback entirely.
  - If the primary result set is non-empty, returns immediately with `used_fallback = false`.
  - If empty and `album` is `Some` and `fallback_enabled`: performs the album-only search (which
    deduplicates via the shared `search_raw` helper, same rules as the primary), then per Section 2
    drops non-artist-matching files from every result and discards results left empty, and returns
    with `used_fallback = true` and `fallback_duration_ms` set.
- `pub fn path_matches_artist(path: &str, artist: &str) -> bool` — pure, as specified in Section 2.
  A blank artist returns `false` (an empty artist must never match every path).
- The existing dedup loop is extracted into a small private helper shared by both searches.
- The existing `search_album` function signature and behaviour are unchanged; it remains the
  single-query primitive.

### `runner.rs` (changes)

- Replace the `search::search_album(...)` call in `process_album` with
  `search::search_album_with_fallback(..., config.search.fallback_search)`.
- Add an INFO log when the fallback fires, reporting how many fallback results matched the artist
  (or that none matched).
- The zero-result / filter-fail / download / organize / notify logic is otherwise untouched.

### `config.rs` (changes)

- `SearchConfig` gains `fallback_search: bool` with `#[serde(default = ...)]` set to `true` so
  existing config files without the key keep the feature enabled.
- The default `seakarr.yml` template gains `fallback_search: true` under `search:`.

### `db.rs` (no schema change)

- `search::record_search` (currently defined but never called) is wired into `process_album`: the
  primary search is recorded with its raw result count, and when the fallback fires it is recorded as
  its own row (same `artist`/`album`, `result_count` = number of artist-matching results). Fallback
  usage is visible as additional rows. No migration needed — the table already allows multiple rows
  per album.

### `client.rs` (test infrastructure only)

- `MockClient` gains `search_queries: Mutex<Vec<String>>` recording every query string so tests can
  assert whether a second (fallback) search was issued. Production client code is unchanged.

## 4. Data flow

```text
runner.process_album
  └─ search_album_with_fallback
       ├─ search("Artist Album") ── non-empty → outcome (used_fallback = false)
       └─ empty + album present + enabled
            └─ search("Album")
                 → keep results whose files' paths match artist
                 → outcome (used_fallback = true)
  → filter_results → rank_candidates → download_album → organize → notify   [unchanged]
```

## 5. Error handling

- A fallback search error propagates as `Err`, exactly like a primary search error. Semantics stay
  consistent; transient failures retry on the next daemon cycle.
- An empty fallback result set behaves exactly like today's zero-result primary search: album marked
  `skipped`, `Ok` returned.
- Path matching is pure string processing on untrusted remote data and cannot panic; paths without the
  artist simply do not match.

## 6. Config surface

One new key, no CLI changes:

```yaml
search:
  fallback_search: true   # default; set false to disable the album-only fallback
```

## 7. Testing

- **Unit (`search.rs`):** `path_matches_artist` — backslash paths, case-insensitivity, reordered
  artist names, `The Beatles` vs `Beatles`, punctuation (`AC/DC`), all-stop-word artist (`The The`),
  non-matching paths.
- **Unit (`search.rs`):** `search_album_with_fallback` with `MockClient` —
  - empty primary triggers fallback and artist filtering;
  - non-empty primary issues no second query (assert via `search_queries`);
  - `fallback_enabled = false` skips the fallback;
  - `album = None` skips the fallback.
- **Integration (`runner.rs`):** full `process_album` flow where the primary search is empty and the
  fallback yields a matching result → download completes and the album is marked `success`; fallback
  with zero artist matches → album marked `skipped`. Both cases assert `search_history` rows (two rows
  when the fallback fires: primary with 0 results, fallback with its matched count).
- **Config:** `fallback_search` defaults to `true` and round-trips YAML.

## 8. Out of scope

- Fuzzy/similarity artist matching (edit distance).
- Fallback when the primary search returns results that all fail filters.
- A configurable minimum-result threshold for triggering the fallback.
- Schema changes to `search_history` (a `fallback` column was considered; separate rows provide the
  same visibility without a migration).
