# MusicBrainz library gap filling (`discover` mode)

## Problem

seakarr answers two different questions today, and neither one is "what am I
missing?".

Auto mode scans the library and upgrades albums that already exist but fail the
quality gate. It never asks MusicBrainz anything, and it can only act on an
album it has already found on disk.

Artist-only manual mode does perform authoritative MusicBrainz discovery: it
resolves an artist, selects release groups matching `discography.allowed_types`,
and runs one targeted Soulseek search per eligible album. Two things stop it
from answering the library-wide question:

1. The artist must be named explicitly on the CLI or in `search.manual`. Nothing
   derives a work list from the library.
2. It never consults the library. The only skip is a `processed_albums` success
   record, so an album that is present on disk but was never fetched by seakarr
   (any pre-existing part of the library) is treated as a target and
   re-downloaded.

The result is that a library with 200 artists cannot be filled in from its own
contents, and a named artist run wastes searches and downloads on albums that
are already on disk.

## Goal

Add a fourth mode, `discover`, that:

- derives its artist list from the library itself;
- asks MusicBrainz for each artist's conceptual release groups;
- filters out every album already present in the library;
- downloads the remainder through the existing search, ranking, download,
  organisation, and notification pipeline;
- bounds each run with a configurable download budget, so a large first run
  makes incremental progress instead of queueing hundreds of albums.

Auto mode's behaviour is unchanged, and artist-only manual mode gains the same
library-presence skip so that "missing" means one thing everywhere.

## Scope

In scope:

- a new `discover` mode selectable by `--mode discover` and
  `search.default_mode: discover`, including scheduled mode;
- a library artist and album index built from one recursive walk;
- presence filtering of discovered albums, shared with artist-only manual mode;
- an optional `--artist` filter limited to artists present in the library;
- aggregator exclusions and a per-run download budget;
- aggregate run reporting;
- deterministic tests for the new pure logic and for runner integration;
- README documentation of the mode, the configuration, and the single-instance
  limitation.

Out of scope:

- any change to auto mode's upgrade pass, its ordering, or its tests;
- edition-tolerant title matching;
- album completeness thresholds or partial-album repair;
- quality-based re-downloading (replacing lossy files remains auto mode's job);
- automatic MusicBrainz ID discovery for unresolved artists;
- per-artist caps, year ranges, or a second metadata provider;
- integration of `--album` or `--batch-file` with discover;
- a persistent artist allowlist in configuration;
- changes to per-album notifications.

## Decisions

### A separate mode, not an extension of auto

`discover` is a fourth mode rather than a second phase of auto mode.

Auto mode means "upgrade what I have". Extending it would silently change that
meaning for every existing installation: a scheduled auto run would begin
downloading albums the user never had, at library scale, on the first cycle
after an upgrade. A distinct mode keeps both meanings intact and lets the user
choose which job a scheduled instance performs.

The consequence is documented rather than engineered away: auto and discover
cannot be scheduled concurrently. The PID lock is a single global file
(`pid.file`, default `seakarr.pid`) acquired before Soulseek login, and a second
login with the same username triggers a session takeover that permanently
displaces the running instance. One installation therefore runs one scheduled
plan at a time.

### Artist source: tag artist with folder fallback

The artist list comes from the same derivation the library scanner already uses
for the upgrade pass: the embedded tag artist when readable, otherwise the
folder name. This keeps one definition of "an artist in my library" across
modes, tolerates messy folders, and introduces no new inference logic. The
scanner's known limitation is inherited unchanged: for a nested layout such as
`<root>/Genre/Artist/Album`, the folder-derived value is the genre, which only
matters when tags are missing or unreadable.

### Presence: a matching folder with any audio file

An album counts as present when the index contains a normalised title match for
that artist. The presence proof is the existence of at least one recognised
audio file under that artist/album directory; file contents, quality, and count
are not considered.

This is deliberately simple and predictable. It also means:

- a partially populated album (for example 2 of 12 tracks) counts as present and
  is never completed;
- a deluxe-only or remaster-only copy of an album does not satisfy the plain
  album, so the plain edition may be downloaded even though an edition exists.

Both are accepted consequences of this design, and both are listed under
Deliberate limits.

### Presence matching is strict and normalised

Matching uses the same Unicode NFKC, lowercase, and whitespace-collapse
normalisation already used for MusicBrainz catalog keys. Punctuation remains
significant, exactly as it is for artist-name resolution, so distinct titles
are never conflated.

| MusicBrainz title | Library folder         | Result  |
| ----------------- | ---------------------- | ------- |
| Discovery         | Discovery              | present |
| Discovery         | discovery              | present |
| Discovery         | Discovery (Deluxe)     | missing |
| Discovery         | Discovery [Remastered] | missing |
| Discovery         | Discovery Live         | missing |

### Presence index from one recursive walk

Presence is answered from an index built by a single recursive walk of
`library.paths`, reusing `scanner::scan_library`. The walk already reads tags,
groups files into albums, and handles nesting, so it is correct for
`<root>/Artist/Album` and for `<root>/Genre/Artist/Album` alike.

The alternative, reusing `search::get_library_track_filenames` per candidate
album, was rejected: it resolves only `<root>/Artist/Album` plus a one-level
case-insensitive artist match, so a nested layout reports false misses and the
album is re-downloaded on every run until the budget is exhausted. It also
performs a filesystem walk per album rather than one per run.

### Release types come from `discography.allowed_types`

No new release-type key is introduced. `discover` reads the same
`discography.allowed_types` list as artist-only manual mode, so "the kinds of
music I want" has one source of truth. Every documented classification rule
carries over unchanged, including that a release with several secondary
classifications must have all of them allowed, and that `dj_mix` and `mixtape`
match only a release whose primary type is album, EP, or single.

### Download budget

`discover.max_cycle_downloads` caps the number of download attempts per run.
`0` means unlimited.

The budget is charged when a download attempt begins: search returned at least
one admissible candidate after filtering, and the download stage was entered.
Presence skips and already-processed skips cost nothing. A failed transfer
consumes budget and is retried on a later run, because only successes write
processed-album records; the alternative, charging only on success, makes a bad
night of failed transfers unbounded.

The charge rule needs one explicit distinction. An album whose search produced
nothing, or nothing that survived filtering, never reached the download stage
and must not be charged. Charging it would livelock the feature: failed albums
do not block retries (`is_album_processed` returns true only for `success`), so
with a deterministic album order the same cap-exhausting albums would consume
the budget on every run and no download would ever start. The distinction is
made explicit rather than inferred from reason strings: `AlbumOutcome` gains
`NoCandidates { reason }` for those two paths, and the budget is charged for
every outcome except that one.

`NoCandidates` is an internal discriminant, not a new user-visible outcome. The
run report renders it in the existing `Failed` section with its reason, so
auto, manual, batch, and artist-only output is unchanged.

When the budget is exhausted, the run stops examining further artists instead of
continuing MusicBrainz lookups it cannot act on. The report therefore states how
many of the selected artists were examined and which artist the run stopped at,
rather than claiming a total number of remaining gaps it has not discovered.

### Unresolved artists are skipped and reported

An artist that MusicBrainz cannot resolve is skipped for the run and reported.
`discover` never issues a broad Soulseek artist search and never falls back to
folder-derived album discovery.

This is the single deliberate divergence from artist-only manual mode, which
falls back to the legacy heuristic. At library scale that fallback would issue
one broad Soulseek search per unresolved artist and could download albums
identified only by peer folder names, which is the misidentification risk the
authoritative design exists to remove. `discography.artist_mbids` remains the
supported fix for a persistently unresolved artist.

### Provider errors skip rather than fall back

A provider error (transport failure, HTTP status, oversized or malformed
response, incomplete pagination) is reported as a skipped artist for that run.
Cached discographies are unaffected, so an outage only costs the artists that
need a network refresh.

If three consecutive uncached artists fail a provider request, the run aborts
with an error instead of grinding through the remainder of the library against a
dead API.

### Deterministic artist ordering and spelling

Artists are processed in normalised-key order. Where the library spells one
artist several ways, the spelling covering the most albums is the one querying
MusicBrainz, and ties break alphabetically. This makes the query string
deterministic instead of dependent on filesystem walk order.

Within an artist, albums keep the order authoritative discovery already
produces: dated albums oldest to newest, undated albums last sorted by
normalised title, then MusicBrainz ID.

### Exclusions are exact normalised matches

`discover.exclude_artists` entries are compared as exact normalised keys, not
substrings, so an entry can never silently drop an unrelated artist whose name
happens to contain it. The default covers the common aggregator names that would
otherwise expand into hundreds of releases if `compilation` or `live_album` were
enabled.

### `--artist` is an optional narrowing filter

`--mode discover` with no selector walks every eligible library artist.
`--mode discover --artist "Name"` narrows the run to that one artist. The name
must exist in the library index, otherwise the run fails with a configuration
error naming the artist; a typo can never silently do nothing.

The selector can only narrow the library-derived list, never add to it; fetching
a discography for an artist who is not in the library remains artist-only manual
mode's job. `search.manual.artist` is not consulted by discover, consistent with
mode resolution already ignoring inactive configuration values.

`--album` and `--batch-file` remain incompatible with discover and are rejected
with discover-specific messages.

### Presence wins over `--ignore-processed`

`--ignore-processed` bypasses processed-album records only. It never bypasses
the library index, because its documented meaning is "reprocess despite a
processed record", not "ignore my library". `--mode discover --ignore-processed`
therefore cannot re-download an album that is present on disk. Scheduled mode
continues to reject the flag outright.

### Reporting uses the existing notice mechanism

Aggregate facts are emitted through `RunReport::add_notice`, which already
renders ahead of the outcome sections, so `report.rs` needs no new API for
accounting.
Present albums are counted and reported as a single aggregate line rather than
per-album `Skipped` rows, so a 400-album library does not print 400 lines. Only
genuine processed-record skips appear as per-album rows, as they do today.

## Configuration

New block:

```yaml
discover:
  max_cycle_downloads: 5
  exclude_artists: [Various Artists, VA, Unknown Artist]
```

- `discover.max_cycle_downloads` - download attempts per run. `0` means
  unlimited. Default `5`.
- `discover.exclude_artists` - artist keys skipped before any lookup, matched
  exactly. Default `[Various Artists, VA, Unknown Artist]`.

Reused unchanged: `library.paths`, `discography.enabled`,
`discography.cache_days`, `discography.allowed_types`, `discography.artist_mbids`,
`filters.*`, `download.*`, `storage.*`, `schedule.*`, `search.timeout_secs`,
`search.peer_reputation`.

Validation, evaluated during mode resolution before startup side effects:

1. `discover` with `discography.enabled: false` is a configuration error.
   Discover has no meaningful non-authoritative form, so silently using the
   folder heuristic is not an option.
2. `discover` with an empty `library.paths` is a configuration error, mirroring
   auto mode.
3. `discover` with `--album` or `--batch-file` is a configuration error.
4. `--artist` naming an artist absent from the library index is a configuration
   error.

## Architecture

### `src/mode.rs`

- `SearchMode` gains `Discover`.
- `ExecutionPlan` gains `Discover { artist: Option<String> }`.
- `resolve_execution_plan` accepts `discover` as a `--mode` value and as
  `search.default_mode`, applies the validation rules above, and keeps every
  existing auto, manual, and batch rule untouched.
- The auto-mode selector conflict hint becomes "use --mode manual or --mode
  discover", because a bare `--artist` now has two legitimate targets.

### `src/discover.rs` (new)

Pure, synchronous logic only; no I/O beyond the scan it is handed.

- `LibraryIndex` - normalised artist key to a set of normalised album titles,
  plus the original artist spellings and their album counts.
- `build_index(albums: &[ScannedAlbum]) -> LibraryIndex`.
- `missing_albums(&index, artist, targets: &[AlbumTarget]) -> Vec<AlbumTarget>`
  - the presence decision, order-preserving.
- `select_artists(&index, excludes: &[String], filter: Option<&str>)` -
  normalised-order artist list with exclusions and the optional filter applied;
  returns a configuration error when the filter names an artist the index does
  not contain.
- `DownloadBudget` - `new(limit: u32)`, `charge()`, `exhausted()`, with `0`
  meaning unlimited.

### `src/runner.rs`

- `run_discover_mode(client, config, db, artist_filter, ignore_processed)`:
  one `scanner::scan_library` walk, index construction, artist selection, then
  per artist: `discover_artist_albums` (existing, cache-aware), presence filter,
  and per missing album the existing `process_album` path with
  `target_library_path: None` (no upgrade copy) and `library_track_count: None`
  (no library baseline exists for an album that is not present).
  Cancellation, staging, progress display, organisation, notifications, and
  per-album recording are the existing machinery, unchanged.
- `run_artist_only_mode_with_provider` additionally filters its authoritative
  target list through `missing_albums` before `process_artist_album_work`, and
  reports an aggregate notice. This is the only change to existing behaviour in
  this design.
- `AlbumOutcome` gains `NoCandidates { reason }`, produced by the two
  "no results found" and "no results passed filters" paths. `RunReport` renders
  it in the `Failed` section with the same reason text, so no existing summary
  changes. The variant exists so `discover` can distinguish an album that never
  reached the download stage from one that did.

### `src/main.rs`

- CLI help text becomes `auto|manual|batch|discover`.
- `dispatch_execution_plan` gains the `Discover` arm, so one-shot and scheduled
  execution share the same validated plan.

### `src/config.rs`

- `DiscoverConfig` with the two keys and their defaults, wired into `Config`,
  the default YAML template, and the merge-with-defaults path.

## Data flow

1. `scan_library(library.paths, filters)` walks the library once.
2. `build_index` turns the scanned albums into the presence index.
3. `select_artists` applies exclusions and the optional filter, and orders the
   result.
4. For each artist, while the budget is not exhausted:

   a. `discover_artist_albums(provider, db, artist, discography_config)`
      returns authoritative albums from a fresh cache, the network, or a stale
      cache, or reports the artist as unresolvable or failed.
   b. `missing_albums` removes every album already present.
   c. For each remaining album, oldest first: run `process_album`, which
      searches, ranks, downloads, organises, and notifies exactly as it does
      elsewhere, and charge the budget unless the outcome is `NoCandidates`.

5. Aggregate counts are added to the run report as notices, and the summary is
   printed once.

## Reporting contract

Notices emitted by a discover run, in this order when present:

- `discover: <N> album(s) already present; skipped`
- `discover: excluded <N> artist(s) by discover.exclude_artists`
- `discover: <N> artist(s) unresolved on MusicBrainz: <name>, ...` (first ten
  names, then `and N more`)
- `discover: <N> artist(s) had no albums matching discography.allowed_types`
- `discover: provider failed for <name> (<reason>); artist skipped`
- `discover: download budget of <N> reached at artist "<name>"; <X> of <Y>
  artist(s) examined`

Artist-only manual runs additionally emit:

- `Artist-only run: <N> album(s) already present; skipped`
- `Artist-only run: all <N> eligible album(s) already present`

When the provider circuit breaker aborts a run, the notices collected so far
are printed before the error propagates to the CLI, so the artists already
examined remain visible.

Per-album outcome sections and per-album downloads are unchanged.

## Error handling

- **Artist unresolved by MusicBrainz** - skip and report; no broad Soulseek
  search and no legacy fallback.
- **Provider error, cached discography available** - use the stale cache and
  warn, as today.
- **Provider error, no cache** - skip and report the artist.
- **Three consecutive provider errors** - abort the run with the failure.
- **Artist resolved, no eligible albums** - not an error; counted.
- **Album present in the library** - not an error; counted and skipped without
  searching.
- **Album with no admissible candidates** - reported as failed; budget not
  charged and the run continues.
- **`discography.enabled: false`** - configuration error at mode resolution.
- **Empty `library.paths`** - configuration error at mode resolution.
- **`--artist` not found in the library** - configuration error naming the
  artist.
- **`--album` or `--batch-file` with discover** - configuration error with a
  discover-specific message.
- **Cancellation (SIGINT)** - existing behaviour: stop before the next album and
  clean staging.

## Backward compatibility

- Auto mode is unchanged: same targets, same ordering, same tests.
- Artist-only manual mode changes only by skipping albums already present in the
  library. Explaining that to a user needs one README line; the benefit is that
  `--artist X` stops re-downloading albums that are already on disk.
- Explicit `--artist X --album Y` manual runs and batch runs are unchanged: an
  explicit request is an instruction to fetch, not a discovery decision.
- Existing configuration files remain valid; the new block is optional and
  defaulted, and no existing key changes meaning or default.
- Existing database schema is unchanged. Discover writes the same
  processed-album records as every other mode, and reads the same discography
  cache.
- Existing summaries are unchanged: `NoCandidates` renders in the `Failed`
  section with its current reason text.
- Scheduled mode needs no change: it dispatches the same validated plan.

## Deliberate limits

- **No upgrade pass.** Auto mode remains the only upgrade path, so auto and
  discover cannot run as concurrent schedules on one installation.
- **No edition tolerance.** A deluxe-only or remaster-only copy reads as
  missing, and the plain edition may be fetched. Revisitable if it proves noisy.
- **No completeness threshold.** A 2-of-12 album counts as present.
- **No automatic MBID discovery.** `discography.artist_mbids` is the fix for an
  artist MusicBrainz cannot resolve automatically.
- **No per-artist cap.** A single prolific artist can consume the whole
  `max_cycle_downloads` budget in one run.
- **No quality judgement.** Discover never replaces a lossy or low-bitrate
  album it considers present.
- **No second provider.** Discogs and others remain future work.

## Testing

### Pure `discover.rs` tests

- Index construction: tag artist preferred over folder name; folder fallback
  when tags are absent; nested layout resolves the artist from tags; several
  spellings of one artist collapse into one key and the most common spelling
  wins; ties break alphabetically; an empty library yields an empty index.
- Presence matching: exact match present; case, whitespace, and Unicode
  differences present; punctuation differences missing; deluxe and remastered
  qualifiers missing; a title belonging to another artist is missing; input
  order preserved.
- Artist selection: exclusions matched exactly, not as substrings; ordering by
  normalised key; the optional filter selects exactly one artist; an unknown
  filter artist is an error.
- Budget: `0` never exhausts; charging decrements; exhaustion is reported;
  charging stops further work; `NoCandidates` never charges.

### Mode and CLI tests

- `discover` resolves alone from `--mode` and from `search.default_mode`.
- `discover` rejects `--album` and `--batch-file` with discover-specific text.
- `discover` accepts `--artist` and yields `ExecutionPlan::Discover` with the
  artist.
- `discover` with `discography.enabled: false` is a configuration error.
- `discover` with empty `library.paths` is a configuration error.
- Invalid mode strings still list the accepted modes, now including discover.
- Auto mode's selector conflict hint mentions both `manual` and `discover`.

### Runner tests (existing `MockClient` and mock provider)

- An album present in the library causes no search and no download.
- A present album does not consume the budget.
- The budget stops the run after N attempts, and artists after the stopping
  point are never examined.
- An album with no admissible candidates does not consume budget and does not
  prevent later albums in the same artist from being attempted.
- An unresolved artist produces no broad artist search and no download.
- A provider error produces a skip, not a legacy fallback.
- Three consecutive provider errors abort the run.
- A successful download emits the usual notification.
- Artist-only manual skips present albums and reports the aggregate notice.
- Artist-only manual with every album present issues no search at all.
- Ordering: artists in normalised-key order, albums oldest first.

### Regression

- All existing auto-mode, manual-mode, batch-mode, scheduled-mode, and
  artist-only discovery tests continue to pass unchanged, except where the
  artist-only presence skip is the intended new behaviour.

## Acceptance criteria

1. `--mode discover` and `search.default_mode: discover` both run gap filling;
   auto mode's behaviour and existing auto-mode tests are untouched.
2. Artists are derived from library tags with folder fallback, aggregator
   exclusions are applied, and the work order is deterministic.
3. Albums already present under a matching folder are never searched or
   downloaded, in both discover and artist-only manual modes.
4. Release types are honoured exactly from `discography.allowed_types`, with no
   new release-type configuration.
5. At most `discover.max_cycle_downloads` download attempts start per run,
   `0` means unlimited, and neither skipped work nor an album with no admissible
   candidates charges the budget.
6. Unresolved or provider-failed artists are reported and never broad-searched;
   three consecutive provider errors abort the run.
7. Per-album notifications are unchanged, and the run summary carries the
   aggregate accounting, including the artist at which the budget stopped.
8. README documents the mode, both configuration keys, the `--artist` filter,
   and the single-instance limitation on scheduling auto and discover together.

## External contracts

- MusicBrainz: unchanged. Discover reuses the existing provider, its
  one-request-per-second pacing, its `User-Agent`, and its 30-day cache.
- Soulseek: unchanged. Discover issues the same targeted per-album searches as
  artist-only manual mode, and never a broad artist search.
- Database: unchanged schema. Discover writes processed-album rows through the
  existing download path and reads the existing discography cache.
- Notifications: unchanged, one per successfully downloaded album.

## References

- `docs/agent/specs/2026-09-10-authoritative-artist-discography-design.md`
- `docs/agent/specs/2026-09-13-dominant-musicbrainz-artist-resolution-design.md`
- `docs/agent/specs/2026-09-04-ignore-processed-design.md`
- `src/scanner.rs`, `src/discography/mod.rs`, `src/runner.rs`, `src/mode.rs`
