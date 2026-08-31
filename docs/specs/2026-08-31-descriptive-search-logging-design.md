# Descriptive search-tier logging — design

Date: 2026-08-31
Status: approved

## Overview

Make the search-fallback cascade self-explanatory in the log. Today each tier
logs the bare query via the vendored crate ("Searching for nirvana incesticide")
with no indication of which tier it is. Replace that with one descriptive line
per tier from seakarr itself, and demote the vendored line to debug.

## Behaviour

`src/search.rs` logs one descriptive line before each search tier:

| Tier | Log line |
|---|---|
| 1a primary (artist + album) | `Searching for Artist + Album ({artist} {album})` |
| 1a primary (artist only) | `Searching for Artist ({artist})` |
| 1a primary (album only, no artist) | `Searching for Album ({album})` |
| 1b lowercase fallback | `Searching for Artist + Album lowercase ({artist_lower} {album_lower})` |
| 2 album-name-only fallback | `Searching for Album ({album})` |
| Track-title fallback | `Searching by track title ({query})` |

The track-title line complements the existing runner context line
("no usable primary results, falling back to track-title search").

The vendored crate's `info!("Searching for {}", query)`
(`vendor/soulseek-rs-lib/src/client/search.rs`) is demoted to `debug!`, so the
raw query remains available for troubleshooting but no longer repeats at INFO.

## Components

1. `src/search.rs` — `search_album_with_fallback` gains the tier-1a/1b/2 log
   lines (the tier-1a label varies by whether artist/album are present);
   `search_by_title` gains the track-title line.
2. `vendor/soulseek-rs-lib/src/client/search.rs` — one-line change: `info!` →
   `debug!`.

## Scope

Log-wording only: no schema, config, public-API, or behaviour changes. No new
tests (nothing asserts on log text); all 496 existing tests must stay green.

## Documentation

None — the README does not document log lines.
