# Filter-aware search cascade — design

Date: 2026-08-31
Status: approved

## Overview

Today the search cascade (`search_album_with_fallback`) stops at the first tier
whose search returns *any* raw results. When that tier's results are 100% junk
(e.g. an mp3 flood when only flac is wanted), the cascade never reaches the
lowercase, punctuation-normalised, or album-only tiers — even though those
queries might surface the flac copies under different casing/punctuation.

Observed live: `Jimi Hendrix Best Of Jimi Hendrix` returned 2830 files from 129
users, 0 passed filters (1610 rejected as mp3) — and no fallback tier fired.

This change makes the cascade **filter-aware**: each tier continues to the next
when its results survive filtering down to *zero downloadable tracks*, and the
cascade returns the first tier whose results pass the full filter.

## Behaviour

`search_album_with_fallback` gains the filter inputs:

```rust
pub async fn search_album_with_fallback(
    client: &dyn SoulseekClient,
    artist: &str,
    album: Option<&str>,
    timeout_secs: u64,
    filters: &FilterConfig,
    library_track_count: Option<usize>,
) -> Result<SearchOutcome>
```

Per tier (1a original → 1b lowercase → 1c punctuation-normalised → 2
album-only), unchanged except for the continuation decision:

1. Run the tier's search (Tier 2 keeps its internal `path_matches_artist`
   pre-filter as today).
2. If the tier's output is **empty** → continue to the next tier.
3. Otherwise **probe**: run
   `filter::filter_results(&tier_output, filters, library_track_count, Some(album))`.
   - Probe **non-empty** → return this tier's output. The runner's own filter
     pass reproduces the same result (idempotent — same inputs, same output).
   - Probe **empty** → remember this tier's output as `fallback` (first
     non-empty tier) and continue.

End of cascade:

- A tier passed the probe → its results are returned (the new behaviour).
- **No tier passed** → return the first non-empty tier's results (today's
  behaviour). The runner filters them to empty, the existing **title-search
  fallback still fires**, and the rejection summary still has raw data.
- All tiers raw-empty → empty outcome (today).

Always-on: no new config key.

## Rationale

- **Full filter, not a cheap format probe**: the user explicitly chose the
  full `filter_results` as the gate — "if after filtering there are no tracks
  to download then we should start using the fallback searching". A
  format-only probe would still stop the cascade on results that fail slots /
  min-tracks / album-gate checks.
- **Runner stays unchanged except the call site**: the cascade still returns
  raw results, so the runner's stats, `filter_results`, rejection summary, and
  title-search fallback all work as-is. The winning tier's results are
  filtered twice (probe + runner) — a deliberate, cheap redundancy that keeps
  the runner untouched.
- **No tier passes → first tier's results**: preserves the rejection summary
  and the title-search fallback trigger; an empty return would silently lose
  the "2830 files from 129 users" diagnostics.
- **Stats now describe the winning tier**: `total_results`/`total_users` and
  the rejection summary will reflect the tier whose results were actually
  used (e.g. `12 files from 2 users, 8 passed filters`), which is more
  informative than always reporting the first tier.

## Scope

- `src/search.rs`: signature change; per-tier probe + fallback tracking; doc
  comment updates; test updates (new signature) and new tests.
- `src/runner.rs`: call site gains `&config.filters` and `library_track_count`
  (one line).
- No config, schema, or vendored-crate changes. No import cycle:
  `filter.rs` does not depend on `search.rs`.

## Behavioural notes

- Worst case for an album where every tier is all-junk: up to 4 searches
  (~15 s each) instead of 1 — the accepted cost of "all fallback searches
  done" (always-on).
- Empty `allowed_extensions` (broken config): every probe fails, so the
  cascade runs all tiers and returns the first tier's results — degrades to
  today's behaviour plus extra searches.
- Title-search fallback is unaffected (fires on the runner's filtered-empty
  result as before, requires the album in the local library).

## Testing (TDD)

- **Update existing tests**: all direct `search_album_with_fallback` callers
  gain a `FilterConfig` (pattern: `FilterConfig { allowed_extensions:
  vec!["flac".into()], ... }` as in `filter.rs` tests). Existing `.flac` mock
  files pass the probe, so current expectations stay valid.
- **New cascade tests** (MockClient, `search_results_by_query`):
  1. Tier 1a returns only mp3 (probe empty) → cascade continues → a later tier
     (e.g. Tier 1c punctuation-normalised) returns flac → outcome is the later
     tier's results; assert the full query sequence.
  2. Every tier returns only mp3 → outcome is the **first** tier's results
     (today's behaviour), and the full tier sequence ran.
  3. Tier 1a returns flac → single query issued, no fallback tiers.
- Runner/pipeline tests exercise `process_album` and need no signature
  changes; verify they still pass.
