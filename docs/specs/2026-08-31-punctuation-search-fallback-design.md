# Punctuation-normalised search fallback — design

Date: 2026-08-31
Status: approved

## Overview

Soulseek peers often list the same artist/album with different punctuation than
the local tags, so the search query never matches their files. Examples:

- `S.P.Y.` on one side vs `SPY` on the other
- `Guns & Roses` vs `Guns and Roses`
- `In-The-Skys` vs `In The Skys`
- `AC/DC` vs `AC DC`

Today the search cascade sends `artist + " " + album` **verbatim** — the
lowercase and album-only tiers don't touch punctuation, so a punctuation
mismatch produces zero results. This change adds a single punctuation-normalised
artist+album search as an extra fallback tier.

## Behaviour

A new pure function `normalize_search_term(&str) -> String` performs a
case-preserving, accent-folding punctuation normalisation of one artist/album
name:

| Class | Characters | Action |
|---|---|---|
| Word substitution | `&`, `+` | → ` and ` (so `Guns & Roses` → `Guns and Roses`) |
| Join (delete, letters merge) | `.` `,` `'` `"` `` ` `` | deleted (`S.P.Y.` → `SPY`, `D'Angelo` → `DAngelo`) |
| Separator → space | `-` `_` `/` `\` `(` `)` `[` `]` `{` `}` | → space (`In-The-Skys` → `In The Skys`, `AC/DC` → `AC DC`) |
| Accent fold | non-ASCII | NFKD → ASCII (`Tiësto` → `Tiesto`, `Café` → `Cafe`) |
| Case | — | preserved (no lowercasing) |
| Whitespace | — | collapsed and trimmed |

A new tier **1c** is inserted into `search_album_with_fallback`, **after** the
lowercase tier (1b) and **before** the album-only tier (2):

1. Guard: artist non-empty **and** album non-empty (mirrors Tier 1b).
2. Compute `artist_norm = normalize_search_term(artist)` and
   `album_norm = normalize_search_term(album)`.
3. **Skip** when normalisation changed nothing
   (`artist_norm == artist.trim() && album_norm == album.trim()`) — avoids a
   byte-identical duplicate of Tier 1a.
4. Otherwise log
   `Searching for Artist + Album punctuation-normalised (…)` and search
   `"{artist_norm} {album_norm}"` (trimmed).
5. If non-empty → return results (no path filter, like Tier 1a/1b; downstream
   `filter_results` still applies).

Resulting tier order: **1a original → 1b lowercase → 1c punctuation-normalised →
2 album-only**.

## Rationale

- **Single query**: normalising both artist and album into one search covers all
  the reported mismatch types without a combinatorial blow-up of per-character
  fallbacks.
- **Case preserved**: the lowercase tier (1b) already covers the lowercased
  form, and lowercase queries return lower-quality results (MP3s), so the
  punctuation tier keeps the original casing.
- **No artist path-filter here**: it is still an artist+album query (stronger
  than album-only), so it returns raw results like 1a/1b. The normalised query
  is slightly looser, but it still carries both artist and album tokens, so
  wrong-artist matches stay unlikely; the standard `filter_results` pipeline
  still applies.
- **Always-on**: like the existing lowercase tier, this is a cheap fallback with
  no new config surface.

## Scope

- `src/search.rs`: add `normalize_search_term`; insert Tier 1c in
  `search_album_with_fallback`; update the function's doc comment.
- No config, schema, or vendored-crate changes.
- No changes to `clean_track_title` / `search_by_title` (track-title path).

## Testing (TDD)

- **Unit — `normalize_search_term`**:
  - `S.P.Y.` → `SPY`
  - `S.P.Y. - In The Skys` → `SPY In The Skys`
  - `Guns & Roses` → `Guns and Roses`
  - `AC-DC` → `AC DC`
  - `In_The-Skys (Deluxe)` → `In The Skys Deluxe`
  - `D'Angelo` → `DAngelo`
  - `Tiësto` → `Tiesto`
  - case preserved: `Spy In The Skys` unchanged
  - no-change passthrough: `Musicology` unchanged
- **Tier tests (MockClient)**:
  - Tier 1c fires when 1a/1b are empty and normalisation changes the query.
  - Tier 1c is **skipped** when the input has no punctuation (query count
    unchanged — existing 3-query tests for `Artist`/`Album` stay valid).
  - Full query-sequence test asserting `1a → 1b → 1c → album-only`.
