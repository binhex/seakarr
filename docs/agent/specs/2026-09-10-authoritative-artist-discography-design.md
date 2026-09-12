# Authoritative artist discography discovery

## Problem

Artist-only manual mode currently performs one broad Soulseek search and treats
matching directory names as album names. This discovers multiple folders, but
it cannot distinguish official albums from singles, live mixes, remixes,
bootlegs, user-created collections, or other arbitrary share layouts.

The broad artist query also misses results that are returned for a more targeted
`Artist Album` query. This can reduce both result quality and candidate choice.

## Goal

Before artist-only manual mode searches Soulseek, resolve the artist through an
authoritative discography service, obtain the artist's configured conceptual
album types, and process each album sequentially through the existing targeted
artist-plus-album pipeline.

Use MusicBrainz as the initial authoritative source. Preserve the current broad
Soulseek discovery path as an explicit opt-out and a prominently reported
fallback when authoritative discovery cannot be established.

## Scope

In scope:

- all artist-only manual runs, whether selected by CLI, configuration, or a
  repeated daemon execution plan;
- MusicBrainz artist resolution and release-group retrieval;
- configurable release categories with studio albums as the default;
- optional per-artist MusicBrainz ID overrides;
- persistent SQLite discography caching;
- sequential targeted Soulseek searches for every eligible album;
- visible stale-cache and legacy-fallback reporting;
- deterministic provider, cache, runner, and regression tests.

Out of scope:

- explicit artist-plus-album and album-only manual searches;
- batch-file artist expansion;
- automatic library-upgrade discovery;
- querying individual editions, pressings, deluxe releases, or remasters;
- combining or reconciling multiple metadata providers;
- interactive artist selection;
- scraping Amazon Music or another website;
- changing the existing per-album filtering, ranking, download, organization,
  notification, or queue-aware timeout behavior.

This design intentionally supersedes the previous artist-only design's rule
that discovery performs one Soulseek query and does not issue a query per
album.

## Decisions

### Provider

MusicBrainz is the sole authoritative provider in this iteration because it:

- exposes conceptual release groups rather than only individual editions;
- provides primary and secondary release classifications;
- supports JSON without an API credential for normal read access;
- publishes open data and a documented API;
- requires only an identifying User-Agent and one-request-per-second pacing.

Discogs remains a possible future provider, but adding it now would introduce
token handling, a second rate-limit policy, and ambiguous cross-provider
identity reconciliation. Spotify is market-dependent and requires OAuth.
Amazon Music does not provide a suitable supported public discography API.

### Release scope

Release categories are configurable. The default includes only
`studio_album`.

Supported friendly categories are:

- `studio_album`;
- `live_album`;
- `ep`;
- `single`;
- `compilation`;
- `remix`;
- `soundtrack`;
- `dj_mix`;
- `mixtape`.

`studio_album` means MusicBrainz primary type `Album` with no secondary type.
The other values map to the corresponding MusicBrainz primary or secondary
types. A release group with multiple secondary classifications is eligible only
when every recognized classification is allowed. For example, a release group
classified as both live and compilation requires both `live_album` and
`compilation`.

Classification is exact:

- primary `Album` with no secondary type maps to `studio_album`;
- primary `Album` with secondary types maps only to those secondary friendly
  categories and does not also require `studio_album`;
- primary `EP` or `Single` requires `ep` or `single` respectively, plus every
  mapped secondary category when present.

Unknown primary or secondary types fail closed: the release group is excluded
and logged at debug level. Empty release titles are excluded. Release-group
browse requests use MusicBrainz `release-group-status=website-default`, which
excludes groups represented only by promotional, bootleg, or pseudo-releases
while retaining a conceptual album that also has an official release.

### Artist identity

A configured MusicBrainz artist ID is optional and always takes precedence.
Without an override, automatic resolution accepts only one unique exact artist
name match after Unicode NFKC normalization, Unicode lowercase conversion,
trimming, and whitespace collapse. Punctuation remains significant so distinct
names are not widened into false matches. Only the MusicBrainz canonical `name`
field participates; search score, sort name, and aliases never widen automatic
matching.

Zero exact matches or multiple exact matches are unresolved. They do not select
the highest-scored result automatically; the discovery service uses stale cache
when available or the visible legacy fallback otherwise.

### Conceptual album identity and order

The provider consumes MusicBrainz release groups, never individual releases.
Canonical titles are deduplicated using Unicode NFKC normalization, lowercase
conversion, trimming, and whitespace collapse while retaining punctuation. If
multiple release groups normalize to the same title, retain the group with the
earliest known first-release date; use MusicBrainz ID as the stable tie-breaker.

MusicBrainz partial dates `YYYY`, `YYYY-MM`, and `YYYY-MM-DD` are valid. Compare
their numeric year, month, and day components, treating a missing month or day
as zero. Empty, malformed, or impossible dates are undated rather than fatal.
Process dated albums from oldest to newest. Undated albums follow dated albums
and sort by normalized title, then MusicBrainz ID. Every eligible conceptual
album is processed; there is no album-count or year-range limit.

## Configuration

Add this top-level section to the generated and reconciled configuration:

```yaml
discography:
  enabled: true
  cache_days: 30
  allowed_types:
    - studio_album
  artist_mbids: {}
```

Semantics:

- `enabled: true` attempts authoritative discovery for every artist-only manual
  run. `false` explicitly selects the legacy broad Soulseek flow.
- `cache_days` is the number of complete 24-hour periods for which a successful
  lookup remains fresh. `0` always attempts a refresh but retains cached data
  as stale fallback.
- `allowed_types` must be non-empty when discovery is enabled and every value
  must be one of the supported friendly categories.
- `artist_mbids` maps requested artist names to canonical hyphenated MusicBrainz
  UUIDs. Keys are matched after Unicode NFKC normalization, trimming, Unicode
  lowercase conversion, and whitespace collapse.

Malformed MusicBrainz IDs, duplicate normalized mapping keys, unknown release
types, and an empty enabled allowlist are configuration errors detected before
network or database side effects.

No provider credential or secret is added.

## Architecture

### MusicBrainz provider boundary

Add a dedicated `discography` module with an async provider trait. The
production MusicBrainz implementation owns only HTTP behavior and typed wire
models:

- artist search;
- release-group pagination by artist MusicBrainz ID;
- required User-Agent construction;
- response-size and pagination limits;
- request pacing, timeout, and retry behavior.

It does not know about Soulseek, run reports, SQLite, or processed albums. A
fake provider can therefore drive service and runner tests without external
network access.

### Discovery service

The discovery service coordinates configuration, cache, provider data, and
pure transformations. It returns one of these explicit outcomes:

- authoritative albums from fresh cache;
- authoritative albums from a successful refresh;
- authoritative albums from stale cache, including a warning reason;
- legacy fallback, including the exact reason;
- authoritative success with no eligible albums.

The service owns artist matching, classification, deduplication, ordering,
cache precedence, and fallback decisions. It never starts Soulseek searches.

### Runner integration

Artist-only manual mode asks the discovery service for an outcome before any
Soulseek request.

For an authoritative album list, it invokes the existing per-album processing
pipeline sequentially using the user-supplied artist and canonical MusicBrainz
album title. The existing search cascade remains intact: original-case
artist-plus-album, lowercase fallback, punctuation-normalized fallback, and
album-only fallback with artist-path validation.

For a legacy outcome, the runner calls the current one-query artist search and
directory-grouping path unchanged. Setting `discography.enabled: false` also
uses this path, but it is logged as an explicit configuration choice rather
than a provider failure.

## MusicBrainz HTTP contract

The client identifies itself with a User-Agent containing the seakarr version
and project URL. Requests are globally paced to at most one start per second
within the process.

Each request has a 10-second timeout and at most three total attempts. Transport
errors and HTTP 5xx responses retry after 1 second and then 2 seconds. HTTP 429
honors a valid `Retry-After` value up to 30 seconds; a larger or malformed value
is treated as provider unavailability rather than causing an unbounded wait.
Other HTTP 4xx responses are not retried.

Artist and release-group responses are requested as JSON. Release-group browse
requests set `release-group-status=website-default`. Each response body is
limited to 4 MiB before deserialization. Release groups use the provider's
maximum supported page size and at most 100 pages. Every page must contain the
exact expected number of records for its offset and reported total. A malformed
or short page, repeated offset, inconsistent count, payload-count mismatch,
duplicate release-group ID, or page-limit overflow makes the refresh
incomplete. Incomplete data is never
cached or processed as an authoritative complete discography.

Tests use an injected pacing/retry clock or paused Tokio time; they never wait
real seconds.

## Cache design

Add a `discography_cache` SQLite table with:

- normalized requested artist key as the primary key;
- resolved MusicBrainz artist ID;
- canonical MusicBrainz artist name;
- fetch timestamp stored as Unix seconds;
- JSON containing the complete, unfiltered vector of bounded internal
  release-group records.

Each cached release-group record contains only the MusicBrainz ID, title,
first-release date, primary type, and secondary types. It is raw with respect to
seakarr's `allowed_types`, not a verbatim copy of unrelated provider fields.
Changing the allowlist therefore re-filters cached data immediately.

A cache entry is fresh while its age is less than `cache_days * 86,400`
seconds. Boundary equality is stale. Clock rollback or an invalid future
timestamp also makes the row stale. A configured artist ID that differs from
the cached ID bypasses the row and refreshes it.

Cache writes replace one artist entry atomically only after every provider page
has succeeded. A corrupt row is warned about, removed or ignored, and refreshed.
If refresh succeeds but the cache write fails, processing continues with the
fetched authoritative list and emits a warning.

## End-to-end data flow

1. Detect an artist-only manual target.
2. If authoritative discovery is disabled, log the explicit legacy selection
   and run the existing flow.
3. Read the artist cache entry.
4. If it is fresh and compatible with an optional configured artist ID, filter
   and return it without network access.
5. Retain a compatible stale entry while attempting refresh.
6. Resolve the configured artist ID or one unique normalized exact-name match.
7. Fetch every release-group page for the resolved artist.
8. Atomically cache the complete unfiltered records.
9. Apply the configured categories, deduplicate conceptual titles, and order
   albums oldest-first.
10. If the completed authoritative result has no eligible albums, report that
    outcome and issue no Soulseek search.
11. Otherwise, process every eligible album sequentially through the existing
    targeted album pipeline.
12. If resolution or refresh fails, use compatible stale cache when available.
13. If no cache is available, emit the legacy warning and summary notice, then
    run the current broad Soulseek discovery path.

Existing processed-album checks run before each targeted search. A previously
successful album is skipped. Each attempted album retains its own search-history
row, staging directory, filtering, candidate fallback, status, notification,
and run-summary entry. One album failure does not block later albums.
Cancellation stops iteration and cleans the active staging directory through
the existing path.

## Error handling and observability

Provider failures are typed and retain enough context to distinguish artist
resolution, HTTP status, timeout, response size, malformed JSON, and incomplete
pagination.

Fallback precedence is:

1. fresh authoritative cache;
2. successful MusicBrainz refresh;
3. stale authoritative cache;
4. legacy Soulseek discovery.

Using stale cache emits a warning with cache age and refresh failure. Automatic
legacy fallback emits one prominent warning before the broad Soulseek search
and adds a final run-summary notice containing the exact reason and stating that
album names were discovered heuristically. It does not emit a warning for the
user's explicit `enabled: false` choice, although the log identifies legacy
mode.

A complete MusicBrainz response with zero release groups, or zero releases left
after configured filtering, is an authoritative empty result. It never falls
back to heuristic discovery and adds a neutral run-summary notice rather than a
failed or skipped album outcome.

The run report gains a general notice mechanism rather than encoding fallback
as a failed album. Notices do not change the process exit status.

## Backward compatibility

The new configuration section is supplied by serde defaults and normal config
reconciliation, so existing configurations continue to load. Authoritative
discovery is enabled by default, intentionally changing artist-only manual
behavior.

Users can restore the previous behavior with `discography.enabled: false`.
Provider failure also preserves availability through the visible automatic
legacy fallback. Explicit album, album-only, batch, auto, and library-upgrade
flows retain their existing behavior and query patterns.

The existing legacy grouping implementation remains covered because it is a
supported fallback, not deleted dead code.

## Deliberate limits

- Artist search accepts at most one complete page of 100 candidates. A larger
  reported result set cannot prove exact-name uniqueness and therefore uses a
  compatible stale cache or visible legacy fallback, even if page one contains
  one exact match.
- Configuration validates MBID syntax, not remote existence. A well-formed ID
  that returns 404 is not negatively cached and is retried on a later
  stale/uncached run before following normal fallback precedence.
- The SQLite cache retains one bounded row per normalized artist with no
  automatic eviction. Refresh atomically replaces that artist's row.
- `Retry-After` supports bounded integer delay-seconds from 0 through 30.
  HTTP-date, malformed, and larger values are treated as provider
  unavailability rather than creating an unbounded wait.

## Testing

### Provider tests

Use `wiremock` and deterministic time to verify:

- unique exact artist lookup and encoded query parameters;
- required User-Agent;
- direct configured-ID release-group lookup;
- pagination, `release-group-status=website-default`, and stable offset
  progression;
- one-request-per-second pacing without wall-clock delay;
- transport, 429, and 5xx retry behavior;
- non-retryable 4xx responses;
- timeout, body-size, malformed-JSON, repeated-offset, inconsistent-count, and
  page-limit failures;
- no partial result escaping an incomplete refresh.

### Pure discovery tests

Table-driven tests cover:

- NFKC/case/whitespace artist equality while retaining punctuation;
- canonical-name-only matching that ignores score, sort name, and aliases;
- zero, one, and multiple exact artist matches;
- every friendly release category;
- conservative multi-secondary classification;
- unknown and empty types;
- empty titles;
- conceptual title deduplication and deterministic tie-breaking;
- complete and partial MusicBrainz dates, malformed dates, oldest-first order,
  and undated ordering.

### Cache and configuration tests

SQLite and config tests cover:

- generated defaults and reconciliation into existing YAML;
- invalid IDs, duplicate normalized override keys, unknown types, and empty
  enabled allowlists;
- fresh cache hits without HTTP calls;
- 30-day boundary and `cache_days: 0` behavior;
- successful stale refresh;
- stale-on-refresh-failure;
- configured-ID cache invalidation;
- corrupt rows and failed cache writes;
- immediate re-filtering when `allowed_types` changes.

### Runner tests

Runner tests prove:

- authoritative success never sends the broad artist-only Soulseek query;
- one sequential targeted search cascade is started per eligible album;
- albums are processed oldest-first;
- successful processed albums are skipped before searching;
- album failures do not block later albums;
- cancellation stops later searches;
- authoritative empty results issue no Soulseek request and create a neutral
  summary notice rather than a failed/skipped album;
- stale cache warnings are visible;
- automatic legacy fallback emits both WARN and a final summary notice;
- explicit disablement uses legacy behavior without an outage warning;
- explicit artist-plus-album, album-only, batch, auto, queue-aware, search
  history, organization, and notification regressions remain green.

## Acceptance criteria

- Artist-only manual mode normally derives targets from MusicBrainz release
  groups before searching Soulseek.
- The default downloads only conceptual studio albums.
- Users can configure supported release categories and optional artist IDs.
- Every eligible album receives a targeted `Artist Album` search, sequentially
  and oldest-first.
- Editions and normalized duplicate titles produce only one target search.
- Fresh and stale cache behavior follows the documented precedence and 30-day
  default.
- Provider ambiguity or failure cannot silently activate heuristic discovery;
  WARN and run-summary output clearly identify automatic legacy fallback.
- A valid authoritative empty result downloads nothing, reports a neutral
  notice, and does not fall back.
- Explicit artist-plus-album, album-only, batch, auto, and library-upgrade modes
  are unchanged.
- Provider and retry tests use no live service and no real multi-second waits.
- Formatting, Clippy, the complete Rust workspace test suite, Markdown lint,
  pre-commit, and diff checks pass before implementation completion.

## External contracts

- [MusicBrainz web service v2](https://musicbrainz.org/doc/Development/XML_Web_Service/Version_2)
- [MusicBrainz API rate limiting](https://musicbrainz.org/doc/MusicBrainz_API/Rate_Limiting)
- [MusicBrainz release-group types](https://musicbrainz.org/doc/Release_Group/Type)
- [MusicBrainz release status](https://musicbrainz.org/doc/Release)
