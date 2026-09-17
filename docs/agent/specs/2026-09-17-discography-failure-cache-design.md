# Discography failure cache

## Problem

Discover mode asks MusicBrainz about every artist in the library on every run,
including artists that can never be resolved. Only a successful refresh is
persisted: `upsert_discography_cache` has exactly one production call site, in
the `Ok` arm of the refresh inside `discover_artist_albums_at`. The error path
(`stale_or_legacy`) writes nothing, so an unresolvable artist is re-attempted on
every single run, at one `search_artists` call each, forever.

Measured on the live install on 2026-09-17:

- `logs/seakarr.log` (92 MB, 2026-08-12 to 2026-09-17) contains 106
  `artist could not be resolved on MusicBrainz` lines covering **32 distinct**
  library entries, and 15 `MusicBrainz unavailable` lines covering 13 distinct
  entries.
- `db/seakarr.db` holds 38 `discography_cache` rows, all comfortably fresh
  (0.02 to 4.05 days old against the configured `cache_days: 30`).
- The 32 unresolvable entries are not artists. They are collaboration credits
  (`A Guy Called Gerald feat. David Simpson`, `Afterlife; Cathy Battistessa`,
  `Amy Winehouse feat. Nas`, `Amon Tobin vs. Cujo`, `Aphex Twin, Luke Vibert`)
  and title folders treated as artists (`40 Licks`, `A Break From The Norm`,
  `1962 - 1966`, `30Hz`, `A Forest Mighty Black`).
- Cache hits are invisible: across the whole 92 MB log, **zero** lines mention
  a cache hit. The success cache works, but nothing in the output shows it, so
  the visible symptom is "MusicBrainz is checked every run".

What is explicitly **not** the problem, and must not be changed: `allowed_types`
is not refetched. The cached payload is the full release-group list (live:
Album 1464, Single 968, EP 274, Broadcast 34, Other 46) while `allowed_types` is
`[studio_album, compilation]`, and the filter runs locally in `select_outcome`
after the cache loads. It costs no API calls and needs no expiry of its own.

## Goal

A discover or artist-only run must not ask MusicBrainz about an artist that an
earlier run already determined cannot be resolved, until that knowledge expires
or the user explicitly overrides it. Nothing else changes: the same artists are
skipped, the same fallbacks fire, the same counters and notices are produced for
everything not served from the new cache.

## Scope

In scope:

- a persistent negative cache for `ArtistUnresolved` only;
- `discography.failure_cache_days`, default 7, with `0` disabling the feature;
- bypass rules for an explicit `--artist` and for a pinned
  `discography.artist_mbids` entry;
- distinct reporting so a cached skip is distinguishable from a fresh failure;
- database, discography, runner, config and documentation coverage.

Out of scope:

- `Provider` failures (transport, HTTP status, decode, pagination). They are
  transient, so they stay uncached and self-healing.
- `allowed_types`, the success cache's payload, and its `cache_days` TTL.
- Fixing why most of the 32 entries are not artists at all (credit parsing, or
  pruning them via `discover.exclude_artists`). This specification only stops
  re-asking about them.
- Any new CLI flag, and any bulk cache-clearing command.

## Decisions

**Which failures to remember: `Unresolved` only.** Given a library name and
MusicBrainz's canonical names, non-resolution is deterministic. A `Provider`
failure is an outage, so remembering it would keep suppressing a healthy
MusicBrainz for days, including after the 503s observed in the live log.

**Storage: a dedicated `discography_failures` table.** The two rows have
different shapes and lifecycles (a failure has no MBID, canonical name or
payload). It needs no migration, and it leaves the success cache's contract
untouched - which matters because `stale_or_legacy` depends on it.

**TTL home and default: `discography.failure_cache_days`, default 7.** It sits
beside `cache_days` and the filter it belongs with. The cache is consulted by
`discover_artist_albums`, which serves discover mode and artist-only runs, so it
is not a discover-only concern. A weekly retry is cheap and turns the set over
roughly monthly.

**Bypasses: an explicit `--artist`, and a pinned MBID.** A pinned MBID makes
resolution deterministic, so honouring a cached failure would silently ignore
explicit user configuration. An explicit `--artist` overriding a skip matches
the existing precedent that `--artist` overrides `exclude_artists`.

**Cached-skip reporting: its own counter and notice line.** The absence of any
cache visibility is what made a working cache look broken in the first place. It
also separates "known unresolvable" from "resolution has regressed".

**Circuit breaker: a cached skip neither increments nor resets it.** Today's
`Unresolved` arm resets the counter because "MusicBrainz answered, so the
provider is reachable". A cached skip carries no such evidence, so it must not be
counted as provider health.

## Configuration

```yaml
discography:
  enabled: true
  cache_days: 30
  failure_cache_days: 7      # new: 0 disables the negative cache entirely
  allowed_types: [studio_album, compilation]
  artist_mbids: {}
```

- `failure_cache_days: u64`, `#[serde(default = "default_discography_failure_cache_days")]`,
  defaulting to `7`.
- `0` disables the feature in both directions: no rows are written and no rows
  are honoured, so enabling it later starts clean rather than inheriting rows
  recorded while it was off.
- The key carries a serde default, so existing configuration files load
  unchanged, and the auto-created sample config gains it through
  `Config::default()`.
- The config round-trip fixtures in `config.rs` assert the full serialised
  configuration and must be extended with the new key.

## Architecture

### Table

Added to the existing `execute_batch` in `db.rs`, so it materialises on fresh and
existing databases without a migration step:

```sql
CREATE TABLE IF NOT EXISTS discography_failures (
    artist_key   TEXT PRIMARY KEY COLLATE NOCASE,
    failure_kind TEXT NOT NULL,
    reason       TEXT NOT NULL,
    recorded_at  INTEGER NOT NULL
);
```

`artist_key` carries the same `COLLATE NOCASE` semantics as
`discography_cache.artist_key`. `failure_kind` is stored as text even though a
single value is written today, so the row is self-describing if a second
cacheable kind is ever added. `recorded_at` uses the same unix-second clock as
`fetched_at`.

### Components

- `DiscographyFailureEntry { artist_key, failure_kind, reason, recorded_at }`.
- `Database::get_discography_failure`, `upsert_discography_failure` and
  `delete_discography_failure`, mirroring the three existing cache helpers
  (`.optional()` read, `ON CONFLICT` upsert, NOCASE delete).
- Freshness reuses the existing `cache_is_fresh(timestamp, days, now)` predicate
  unchanged. It is already a generic "is this timestamp within N days" check, so
  there is no second implementation and no chance of the two TTLs diverging.
- `DiscoveryOutcome::LegacyFallback` gains `from_cache: bool`. No other enum
  changes: `DiscoveryProvenance`, `StaleCache` and `DiscoveryFailure` are
  untouched.
- The entry points take an explicit bypass choice so the call site reads as a
  decision rather than a bare boolean, following the pattern of
  `ArtistComponent::Verbatim` and `ExistingFile::KeepWhenValid` in
  `organizer.rs`:

  ```rust
  enum FailureCacheUse {
      /// Honoured when a fresh, known-kind failure row exists.
      Honour,
      /// Ignored: the lookup goes to the provider regardless.
      Bypass,
  }
  ```

  `discover_artist_albums` and its time-injectable variant take this as an
  argument. Discover mode passes `Honour` for the library-derived artist list and
  `Bypass` when `--artist` selected the artist; artist-only and explicit manual
  runs always pass `Bypass`.

## Data flow

```text
artist_key, configured_mbid
        |
        +-- success row fresh? -----------------> FreshCache              (unchanged)
        |
        +-- failure row fresh, kind known,
        |   failure_cache_days > 0,
        |   and not bypassed? -------------------> LegacyFallback {
        |                                           reason: <stored>,
        |                                           kind: Unresolved,
        |                                           from_cache: true }
        |                                          (no provider call)
        |
        +-- refresh: artist_by_id | search_artists -> release_groups
                |
                +-- Ok  -> upsert success row
                |          delete failure row
                |          select_outcome(Refreshed)                       (unchanged)
                |
                +-- Err -> if ArtistUnresolved (a stable name failure)
                |            and no success row exists
                |              upsert failure row
                |          stale_or_legacy(...)                           (unchanged)
```

Three properties are deliberate.

**The success check stays first.** A live success is never shadowed by the new
table.

**The rows are mutually exclusive by construction.** A failure row is written
only when no success row exists for that key, and a successful refresh deletes
any failure row. A stale-but-usable success row is a legitimate state in this
codebase - it is the `StaleCache` fallback - so without this rule a fresh failure
row could sit beside a usable stale success row and the replay would have to
choose between them. Making the combination impossible is simpler than encoding
that precedence, and the live data shows the 32 unresolvable entries have no
success rows, so the rule costs nothing today.

**Only one enum variant changes.** `DiscoveryOutcome::LegacyFallback` gains
`from_cache`. Because artist-only mode always bypasses, a cached skip can only
reach the discover loop, so the runner change is a single match arm.

## Error handling

Every cache problem fails open. A broken or surprising row must never block
discovery, matching how `load_compatible_cache` already tolerates a corrupt
success row by warning and continuing.

- Read error: warn, treat as a miss.
- Unknown `failure_kind` value (a future kind, or a hand-edited database): treat
  as a miss rather than guessing at its meaning.
- `recorded_at` in the future: stale, matching `cache_is_fresh`.
- Empty `reason`: still honoured, with a generated fallback message so a log
  line is never blank.
- Failure to write or delete a row: warn only, never a run failure, matching how
  a failed `upsert_discography_cache` is already handled.

## Backward compatibility

- Existing databases gain the table through `CREATE TABLE IF NOT EXISTS`; no
  migration and no rebuild.
- An artist with no failure row behaves exactly as today, so the change is inert
  until a failure has been recorded once.
- `discography.cache_days`, the success cache's payload, and `allowed_types` are
  untouched.
- Existing configuration files load unchanged because the new key has a serde
  default.
- `DiscoveryOutcome::LegacyFallback` gaining a field is a compile-time change to
  its constructors and matches, all of which are inside this crate.

## Deliberate limits

- The 32 entries stay in the library and stay visible to the scanner as
  artists. After this change they cost one MusicBrainz search per
  `failure_cache_days` rather than one per run. Removing them from
  consideration entirely is a separate change.
- An artist that previously resolved and later became unresolvable is not
  negatively cached, because a success row exists and the rows are mutually
  exclusive. It therefore keeps its per-run cost. This case does not appear in
  the live data.
- Not every recorded decision is purely a property of the name. Three of the
  `ArtistUnresolved` producers decide from MusicBrainz's per-response search
  scores, so a later re-ranking can make a name resolvable while the cached row
  still suppresses the lookup until the expiry elapses.
- Each artist still costs one database read per run.
- There is no bulk-clear command. The exits from the cache are the TTL, a
  successful resolution, and the pinned-MBID bypass.

## Testing

`db.rs`

- A fresh database contains `discography_failures` (extend the existing table
  list assertion).
- Round-trip, upsert-replaces, and NOCASE key lookup.
- Delete reports whether a row was removed.
- A database created before the table existed gains it on open.

`discography` (using the existing `FakeProvider`, with call counting)

- Honoured fresh row yields `LegacyFallback { from_cache: true }` and makes
  **zero** provider calls.
- A row exactly at the TTL, and one beyond it, both call the provider.
- `failure_cache_days = 0` calls the provider and writes nothing.
- An explicit-artist bypass calls the provider.
- A pinned MBID calls `artist_by_id` and deletes the row.
- A successful refresh deletes an existing failure row.
- A `Provider` error writes no row.
- No row is written when a success row exists.
- An unknown `failure_kind` is treated as a miss.

`runner`

- A cached skip increments the new counter, leaves the `unresolved` counter and
  the consecutive-provider-failure counter untouched, and emits the new notice.
- A fresh `Unresolved` outcome behaves exactly as before, as a regression guard.

`config`

- Default is 7; an explicit value parses; `0` disables.
- The full-YAML round-trip fixtures include the new key.

## Acceptance criteria

1. A second discover run over an unchanged library performs **zero**
   `search_artists` calls for the 32 currently unresolvable entries.
2. An unresolvable entry is retried after `failure_cache_days` and not before.
3. `failure_cache_days: 0` restores today's behaviour exactly, and writes no
   failure rows.
4. `--artist X` always queries MusicBrainz for `X`, even when a fresh failure row
   exists.
5. A pinned `discography.artist_mbids` entry always queries MusicBrainz and
   clears the failure row for that artist.
6. A `Provider` failure never creates a failure row.
7. The run summary distinguishes cached skips from fresh failures, adding a
   line of the form `discover: N artist(s) skipped from cached resolution
   failures` alongside the existing unresolved line.
8. All existing tests pass, and `cargo clippy -- -D warnings`,
   `cargo fmt --check` and `cargo deny check` stay green.

## External contracts

- MusicBrainz: no new endpoint and no new query shape. The change strictly
  reduces request volume.
- SQLite: one new table. No existing table, column or index changes.
- Configuration: one new key, `discography.failure_cache_days`, with a default,
  so the file remains backward compatible.
- Log and report surface: one new notice line in the discover summary, plus a
  DEBUG line per cached skip. Fresh-failure output is unchanged.

## References

- `src/discography/mod.rs` - `discover_artist_albums_at`, `load_compatible_cache`,
  `cache_is_fresh`, `select_outcome`, `stale_or_legacy`, `DiscoveryFailure`,
  `DiscoveryOutcome`.
- `src/discography/musicbrainz.rs` - `search_artists`, `artist_by_id`,
  `release_groups`.
- `src/db.rs` - schema bootstrap, `discography_cache` helpers,
  `DiscographyCacheEntry`.
- `src/runner.rs` - the discover loop's `LegacyFallback` arm and
  `DISCOVER_PROVIDER_FAILURE_LIMIT`.
- `src/config.rs` - `DiscographyConfig`.
- `docs/agent/specs/2026-09-10-authoritative-artist-discography-design.md` -
  the discography resolution and caching design this extends.
- `docs/agent/specs/2026-09-15-musicbrainz-library-gap-filling-design.md` -
  the discover mode this caching serves.
- Live evidence: `logs/seakarr.log` and `db/seakarr.db` in the deployment at
  `/cache-downloads/appdata/data/qbittorrent/completed/release/`.
